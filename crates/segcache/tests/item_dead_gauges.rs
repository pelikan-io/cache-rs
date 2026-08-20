//! `item_dead` / `item_dead_bytes` are OCCUPANCY gauges.
//!
//! They answer "how much dead weight is sitting in segments right now" —
//! fragmentation — not "how many items have ever died". `Segment::remove_item_at`
//! charges a retired item's space to its segment;
//! `SegmentHeader::reset_write_stats` gives the whole charge back when the
//! segment is reclaimed (recycle, re-reserve, or whichever of the three
//! claimants wins the AwaitingRelease -> Free CAS on a condemned segment);
//! a RELOCATION (merge copy-out, S3-FIFO promotion)
//! is neutral, because a moved item did not die.
//!
//! This file pins the three consequences that are visible from outside the
//! crate: a real death shows up, the gauge comes back DOWN when the space is
//! reclaimed (which is the whole change — the previous implementation only
//! ever added), and a segment condemned under a reader pin settles when the
//! pin drops rather than staying charged until something happens to re-reserve
//! it (issue #58 part 2).
//!
//! Relocation-neutrality itself is NOT assertable here: once reclaim exists, a
//! wrong charge on a merge source and its reclaim cancel out microseconds
//! later, so it is invisible at every quiescent point. It is pinned at the
//! per-segment level instead — `src/segments/dead_accounting_tests.rs`.
//!
//! WHY THIS LIVES IN `tests/` AND IS THE ONLY TEST IN THE FILE: the gauges are
//! process-global statics and `libtest` runs one binary's tests concurrently on
//! many threads, so any absolute (or before/after delta) assertion made
//! alongside another test would race it. Each file under `tests/` is its own
//! binary, so exactly ONE `#[test]` here means nothing else in the process can
//! touch the gauges while it runs. Do not add a second test to this file — add
//! another file instead (same rule as `tests/item_gauges.rs`).

#![cfg(all(feature = "metrics", not(feature = "loom")))]

use segcache::{Item, Policy, Segcache};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const TOTAL_SEGMENTS: usize = 24;

/// Read a registered gauge by its exported metric name.
fn gauge(name: &str) -> i64 {
    for metric in metriken::metrics().iter() {
        if metric.name() == name {
            return match metric.value() {
                Some(metriken::Value::Gauge(v)) => v,
                other => panic!("metric {name} is not a gauge: {:?}", other.is_some()),
            };
        }
    }
    panic!("metric {name} is not registered");
}

/// Read a registered counter by its exported metric name.
fn counter(name: &str) -> u64 {
    for metric in metriken::metrics().iter() {
        if metric.name() == name {
            return match metric.value() {
                Some(metriken::Value::Counter(v)) => v,
                other => panic!("metric {name} is not a counter: {:?}", other.is_some()),
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

fn item_size() -> usize {
    keyvalue::item_size(key(0).len(), &keyvalue::Value::Bytes(val(0).as_bytes()), 0)
}

fn segment_size() -> i32 {
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    (magic_overhead + item_size() * ITEMS_PER_SEGMENT) as i32
}

fn cache_with(policy: Policy) -> Segcache {
    Segcache::builder()
        .segment_size(segment_size())
        .heap_size(segment_size() as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        .eviction(policy)
        .build()
        .expect("failed to create cache")
}

/// Drain every segment out of the hashtable, repeating until a pass frees
/// nothing, so every segment that can recycle has recycled (and therefore run
/// `reset_write_stats`).
fn drain_fully(cache: &Segcache) {
    for _ in 0..64 {
        if cache.clear() == 0 {
            break;
        }
    }
    assert_eq!(cache.clear(), 0, "cache must be fully drained");
}

fn assert_dead_gauges_zero(context: &str) {
    assert_eq!(
        gauge("item_dead"),
        0,
        "item_dead did not return to zero {context} — dead space is reclaimed \
         when a segment is, so a non-zero reading here is space charged to a \
         segment that no longer holds it (positive), or a charge given back \
         twice (negative)"
    );
    assert_eq!(
        gauge("item_dead_bytes"),
        0,
        "item_dead_bytes did not return to zero {context}"
    );
}

#[test]
fn item_dead_gauges_track_reclaimable_space() {
    let ttl = Duration::from_secs(3600);
    let heap_bytes = segment_size() as i64 * TOTAL_SEGMENTS as i64;

    // Nothing else runs in this process, so the gauges start clean.
    assert_dead_gauges_zero("at process start");

    // ── Phase 1: a real death is charged, and reclaimed at recycle ──────
    //
    // A roomy FIFO cache: no eviction, no merging, no compaction, so every
    // number below is exact. Deleting an item is the plainest death there is.
    {
        let cache = cache_with(Policy::Fifo);
        let inserted = ITEMS_PER_SEGMENT * 3;
        for i in 0..inserted {
            cache
                .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
                .expect("fill insert");
        }
        assert_dead_gauges_zero("after a fill with no deaths");

        let deleted = 5;
        for i in 0..deleted {
            assert!(cache.delete(key(i).as_bytes()), "delete must find the key");
        }
        assert_eq!(
            gauge("item_dead"),
            deleted as i64,
            "each delete leaves exactly one item's worth of dead space behind"
        );
        assert_eq!(
            gauge("item_dead_bytes"),
            (deleted * item_size()) as i64,
            "dead bytes must be the deleted items' full on-segment size"
        );

        // Recycling every segment hands all of that space back. Under the old
        // add-only implementation this assertion is the one that fails: the
        // gauge would still read `inserted` (every item died on the way out)
        // forever.
        drain_fully(&cache);
        assert_dead_gauges_zero("after draining every segment");
    }

    // ── Phase 2: a merge/promotion storm stays BOUNDED by the heap ──────
    //
    // Merge (copy_into) and S3-FIFO (s3fifo_promote_from) both relocate items,
    // and both run `remove_item_at` on the source to do it. Dead space cannot
    // exceed the space that exists: at any quiescent point the gauge is a sum
    // over segments of space they physically hold, so it is bounded by the
    // heap. A cumulative total is not — this workload retires far more than a
    // heap's worth of items.
    for policy in [
        Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        },
        Policy::S3Fifo {
            admission_ratio: 0.25,
        },
    ] {
        let cache = std::sync::Arc::new(cache_with(policy));
        std::thread::scope(|scope| {
            for t in 0..4 {
                let cache = std::sync::Arc::clone(&cache);
                scope.spawn(move || {
                    for i in 0..8_000 {
                        // A small shared key space: most writes are overwrites
                        // (a death plus an allocation), racing the drains that
                        // are relocating the same keys.
                        let k = i % 64;
                        let _ = cache.insert(key(k).as_bytes(), val(k + t).as_bytes(), None, ttl);
                        let _ = cache.get(key(k).as_bytes());
                        if i % 8 == 0 {
                            let _ = cache.delete(key(k).as_bytes());
                        }
                    }
                });
            }
            for t in 0..2 {
                let cache = std::sync::Arc::clone(&cache);
                scope.spawn(move || {
                    // Fresh keys keep the pool under eviction pressure, so
                    // drains (and their relocations) run continuously.
                    for i in 0..4_000 {
                        let k = 500_000 + t * 100_000 + i;
                        let _ = cache.insert(key(k).as_bytes(), val(k).as_bytes(), None, ttl);
                    }
                });
            }
        });

        let dead_bytes = gauge("item_dead_bytes");
        assert!(
            (0..=heap_bytes).contains(&dead_bytes),
            "item_dead_bytes ({dead_bytes}) is outside the space that exists \
             ({heap_bytes} bytes of segments) — an occupancy gauge cannot \
             exceed the heap it measures, and cannot go negative"
        );
        let dead_items = gauge("item_dead");
        assert!(
            (0..=(TOTAL_SEGMENTS * ITEMS_PER_SEGMENT) as i64).contains(&dead_items),
            "item_dead ({dead_items}) is outside the item capacity of the heap"
        );

        drain_fully(&cache);
        assert_dead_gauges_zero("after draining a merge/promotion storm");
    }

    // ── Phase 3: a segment condemned under a reader pin (issue #58 part 2) ──
    //
    // A drained segment that a reader still pins is not recycled: it is
    // condemned, and the last `SegmentGuard::drop` returns it to the free
    // queue directly, reaching neither `recycle` nor `try_reserve`. Unless
    // that path settles the segment too, its dead charge sits on the gauge
    // until something happens to re-reserve it — indefinitely, for a cache
    // that has stopped writing.
    {
        let cache = cache_with(Policy::Fifo);
        for i in 0..(ITEMS_PER_SEGMENT * 4) {
            cache
                .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
                .expect("fill insert");
        }

        // Pin one item in every segment by holding the `Item`s.
        let pins: Vec<Item> = (0..(ITEMS_PER_SEGMENT * 4))
            .step_by(ITEMS_PER_SEGMENT)
            .map(|i| {
                cache
                    .get(key(i).as_bytes())
                    .expect("pinned lookup must hit")
            })
            .collect();
        assert!(!pins.is_empty());

        let skipped_before = counter("segment_pinned_skip");
        for _ in 0..8 {
            cache.clear();
        }
        assert!(
            counter("segment_pinned_skip") > skipped_before,
            "no segment was condemned, so this phase would prove nothing about \
             the condemned path — the pins did not hold"
        );

        // The condemned segments were emptied by the drain, so their items
        // really did die and really are charged.
        assert!(
            gauge("item_dead") > 0,
            "the drained-but-pinned segments must be carrying dead space, \
             otherwise the settle-on-guard-drop assertion below is vacuous"
        );

        // Dropping the pins runs the guard-drop free path, which is where the
        // charge has to be given back.
        drop(pins);
        drain_fully(&cache);
        assert_dead_gauges_zero("after the last reader of a condemned segment dropped");

        // Live-side reconciliation rides the same reset, so assert it too —
        // but honestly: this one is a COMPANION invariant, not evidence.
        // Neutering the guard-drop reset leaves it green, because the residue
        // it would catch only exists when an unlink skipped its decrement (an
        // unpinned unlink), and this single-threaded phase produces none. The
        // dead assertion above is what actually reddens.
        assert_eq!(
            gauge("item_current"),
            0,
            "item_current did not settle after a condemned segment was freed \
             by its last reader"
        );
    }
}
