//! `Segment::copy_into` must stay gauge-neutral even when it ABORTS.
//!
//! A merge drain relocates a segment's survivors one at a time: each success
//! runs `remove_item_at` on the source (global gauges -1) and bumps only the
//! DESTINATION's header counters, so the site has to re-add to the gauges to
//! stay neutral. If the relink CAS ever loses, `copy_into` returns
//! `RelinkFailure` and abandons the rest of the segment — while every item it
//! already relocated in that same call stays relocated. Compensation that is
//! accumulated during the loop and applied at the tail is therefore lost on
//! that path; compensation applied per item is not.
//!
//! WHY FAULT INJECTION. By #55's remover Dekker pair a pinned replace/delete
//! cannot republish an entry out of a `Draining` segment, so the ONLY way that
//! CAS can lose is an UNPINNED unlink landing in the few tens of nanoseconds
//! between the per-item liveness gate and the CAS. That is not reachable by
//! workload: a tuned storm of ~46k relocations across merge drains with
//! concurrent same-key writers and deleters produced exactly zero. So the
//! abort is driven directly, through the `segcache::fault` knob, which lives
//! behind its own non-default `fault-injection` feature (kept out of `debug`
//! because `debug` is observational and this knob changes behaviour).
//! Skipping the CAS is faithful to losing it — either way the entry stays at
//! the old location, the item stays in the source, and the bytes already
//! written to the destination are orphaned.
//!
//! WHY ITS OWN FILE. The assertion compares a process-global gauge against
//! this cache's own header counters, so nothing else in the process may touch
//! the gauges. Each file under `tests/` is its own binary; keep this the only
//! test in it.

// Needs all three: `fault-injection` for the knob, `debug` for `items()` and
// `check_integrity()`, `metrics` for the gauge under test. Without them this
// compiles to an empty binary rather than failing. CI runs it via the
// "Test copy_into abort accounting (fault injection)" step.
#![cfg(all(
    feature = "fault-injection",
    feature = "debug",
    feature = "metrics",
    not(feature = "loom")
))]

use segcache::{Policy, Segcache};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const TOTAL_SEGMENTS: usize = 32;

fn gauge(name: &str) -> i64 {
    for metric in metriken::metrics().iter() {
        if metric.name() == name {
            return match metric.value() {
                Some(metriken::Value::Gauge(v)) => v,
                _ => panic!("metric {name} is not a gauge"),
            };
        }
    }
    panic!("metric {name} is not registered");
}

fn key(i: usize) -> String {
    format!("k{i:06}")
}

fn val(i: usize) -> String {
    format!("v{i:06}")
}

/// Drive a merge eviction whose `copy_into` aborts partway through, and
/// assert the global item gauge still agrees with the segment headers.
///
/// `ITEM_CURRENT` is maintained in lockstep with the per-segment
/// `live_items` counters — allocation raises both, `remove_item_at` lowers
/// both, a relocation is neutral in both, and `reset_write_stats` reconciles
/// the residue of unpinned unlinks in both. So at any quiesced point
/// `ITEM_CURRENT` must equal the sum of the headers, which is exactly what
/// `Segcache::items()` reports. That is a sharper invariant than "returns to
/// zero after a full drain": it pins the gauge to the headers continuously,
/// and a relocation that dropped its compensation shows up immediately as
/// the gauge running BELOW the headers.
#[test]
fn aborted_copy_into_still_compensates_the_items_it_relocated() {
    let item_size =
        keyvalue::item_size(key(0).len(), &keyvalue::Value::Bytes(val(0).as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Nothing else runs in this process, so the gauges start clean.
    assert_eq!(gauge("item_current"), 0, "gauge must start at zero");

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);

    // Fill the pool so a long chain of sealed, fully-occupied candidates
    // exists for the merge to walk.
    let filled = TOTAL_SEGMENTS * ITEMS_PER_SEGMENT;
    for i in 0..filled {
        cache
            .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
            .expect("fill insert");
    }
    // Raise every item's frequency so the merge's prune pass keeps them and
    // `copy_into` actually has a run of survivors to relocate.
    for _ in 0..4 {
        for i in 0..filled {
            let _ = cache.get(key(i).as_bytes());
        }
    }

    assert_eq!(
        gauge("item_current"),
        cache.items() as i64,
        "gauge must track the headers before the merge"
    );

    // Arm: let a few relinks succeed, then make the next one lose. With
    // fully-occupied candidates those all fall inside one `copy_into` call,
    // so the abort strands compensation for the survivors already moved.
    segcache::fault::fail_relink_after(3);

    // Keep inserting until the pool is exhausted and eviction has to merge.
    for i in 0..(TOTAL_SEGMENTS * ITEMS_PER_SEGMENT * 4) {
        let k = 1_000_000 + i;
        let _ = cache.insert(key(k).as_bytes(), val(k).as_bytes(), None, ttl);
        if !segcache::fault::armed() {
            break;
        }
    }

    assert!(
        !segcache::fault::armed(),
        "the injected relink failure never fired — the merge never reached \
         copy_into with enough survivors, so this test proved nothing"
    );
    // The failure must have landed on a call that had ALREADY relocated
    // items, because those strandable relocations ARE the bug. The arming
    // countdown spans `copy_into` calls, so "3 relinks then fail" does not by
    // itself guarantee they shared a call: a change to prune's retention or
    // to candidate ordering could make the abort land on a call's first item,
    // where a lost tail-block compensation would be zero and this test would
    // pass while proving nothing.
    let stranded = segcache::fault::relinks_before_firing()
        .expect("armed() is false, so the failure fired and must have recorded a tally");
    assert!(
        stranded > 0,
        "the abort landed on the first item of its copy_into call ({stranded} \
         prior relinks), so no compensation was at risk — retune the arming \
         so the failure falls inside a call that already relocated items"
    );
    segcache::fault::disarm();

    assert_eq!(
        gauge("item_current"),
        cache.items() as i64,
        "ITEM_CURRENT drifted from the segment headers across a copy_into \
         that aborted with RelinkFailure — compensation for the items it had \
         already relocated was lost"
    );

    // The engine is still consistent and usable afterwards.
    cache
        .check_integrity()
        .expect("integrity must hold after an aborted merge copy");
    cache
        .insert(b"kzzzzzz", b"vzzzzzz", None, ttl)
        .expect("post-abort insert");
    assert!(cache.get(b"kzzzzzz").is_some());
}
