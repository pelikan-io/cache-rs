// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Deterministic tests for `get_pinned`'s post-pin revalidation retry (#65),
//! and for the second consumer of the same budget: the bounded
//! stale-incarnation arm of `relookup_after_pin_failure` (#50), whose own
//! section is at the bottom of this file.
//!
//! The bug: the retry budget was spent re-racing from scratch. On a mismatch
//! the revalidation lookup has ALREADY returned the key's new location, and
//! the old loop threw it away and looked the key up again — so a key that was
//! republished three times while one `get` worked exhausted the budget and the
//! `get` returned `None` for a key it could see was live. Downstream that is
//! `add` clobbering a live key and `replace` answering NOT_STORED.
//!
//! The race is a two-thread interleaving — a writer must republish the key in
//! the window between a reader's lookup and its revalidation — so a thread
//! pair can only reach it by luck (measured: one false absent every ~700 to
//! ~22,000 gets, and only under 24-way oversubscribed same-key churn). These
//! tests stand a single-threaded hook in for the writer at exactly the two
//! points that matter, via `segcache::revalidation_fault`, which makes the
//! coverage deterministic rather than statistical:
//!
//! - `after_lookup` fires once per FROM-SCRATCH lookup. A hook that
//!   republishes on every firing therefore starves the pre-fix loop forever
//!   (it re-looked-up on every attempt) and fires exactly ONCE against the
//!   converging loop. That difference is the fix, and
//!   `get_converges_instead_of_re_racing_the_lookup` is red before it.
//! - `before_revalidate` fires inside the pin -> revalidate window that the
//!   budget exists to survive, which is the window the fix shrinks but
//!   deliberately does NOT close. The other two tests pin the budget from both
//!   sides, including that it is still a BUDGET: unbounded retry was rejected
//!   (lock-free is not starvation-free, and nothing bounds how long writers
//!   keep rewriting a hot key), so a build that removed the bound would hang
//!   `bounded_giveup_when_every_revalidation_loses` rather than pass it.

use crate::segcache::{revalidation_fault, REVALIDATE_RETRIES};
use crate::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const KEY: &[u8] = b"hot-key";
const TTL: Duration = Duration::from_secs(3600);

/// A cache far larger than these tests can fill, so nothing evicts and the
/// only relocations are the republishes the hooks perform.
fn roomy_cache() -> Arc<Segcache> {
    Arc::new(
        Segcache::builder()
            .segment_size(1024 * 1024)
            .heap_size(64 * 1024 * 1024)
            .hash_power(16)
            .build()
            .expect("failed to create cache"),
    )
}

fn location_of(cache: &Segcache, key: &[u8]) -> Option<Location> {
    let verifier = cache.segments.verifier();
    cache
        .hashtable
        .lookup_no_freq_update(key, &verifier)
        .map(|(location, _freq)| location)
}

/// Build the hook body: republish `KEY` (a full `set`, which is what publishes
/// a NEW location) up to `limit` times, counting firings in `fired`.
///
/// `insert` re-entered from inside `get_pinned` is safe and is exactly what
/// the race needs: the reader holds at most a READER pin, and a replace takes
/// a remover pin, whose counter is independent (`try_pin_remover` never waits
/// on readers). The cache is sized so no eviction — the one thing that would
/// wait on a reader pin — can run.
fn republisher(
    cache: &Arc<Segcache>,
    fired: &Arc<AtomicUsize>,
    limit: usize,
) -> impl Fn() + 'static {
    let cache = Arc::clone(cache);
    let fired = Arc::clone(fired);
    move || {
        let n = fired.load(AtomicOrdering::Relaxed);
        if n >= limit {
            return;
        }
        fired.store(n + 1, AtomicOrdering::Relaxed);
        let value = format!("v{}", n + 1);
        cache
            .insert(KEY, value.as_bytes(), None, TTL)
            .expect("republish must succeed: the cache is far from full");
    }
}

/// **The #65 regression test.** The key is republished after every
/// from-scratch lookup, forever. The pre-fix loop answered every mismatch with
/// another from-scratch lookup, so it re-armed the hook on every attempt and
/// burned its whole budget losing the same race three times — `None` for a key
/// that was live and resolvable throughout. The converging loop follows the
/// location the revalidation lookup already returned, which is itself
/// currently published, so it does exactly ONE from-scratch lookup and settles
/// on the second attempt.
#[test]
fn get_converges_instead_of_re_racing_the_lookup() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");
    let seeded = location_of(&cache, KEY).expect("seeded key must resolve");

    let fired = Arc::new(AtomicUsize::new(0));
    let item = {
        let _hook = revalidation_fault::on_after_lookup(republisher(&cache, &fired, usize::MAX));
        cache.get(KEY)
    };

    let item = item.expect(
        "false absent: the key was republished, never removed, and every lookup \
         resolved it — a bounded retry that re-races from scratch turns that \
         into a miss (#65)",
    );
    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        1,
        "the retry must follow the location the revalidation returned; a second \
         from-scratch lookup means it re-raced from zero"
    );
    assert_eq!(
        item.value(),
        Value::Bytes(b"v1"),
        "the item handed out must be the one the surviving location publishes"
    );
    assert_ne!(
        location_of(&cache, KEY),
        Some(seeded),
        "the republish must actually have moved the key, or this test proves nothing"
    );
}

/// The budget survives `REVALIDATE_RETRIES - 1` republications landing in the
/// pin -> revalidate window itself — the window the fix shrinks but cannot
/// close. 15 consecutive losses is far past anything measured (~1% per
/// attempt), and the get still returns the live item.
#[test]
fn budget_absorbs_republication_inside_the_revalidation_window() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");

    let fired = Arc::new(AtomicUsize::new(0));
    let limit = REVALIDATE_RETRIES - 1;
    let item = {
        let _hook = revalidation_fault::on_before_revalidate(republisher(&cache, &fired, limit));
        cache.get(KEY)
    };

    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        limit,
        "the hook must have spent the whole budget minus one"
    );
    let item = item.expect("the budget must absorb REVALIDATE_RETRIES - 1 mismatches");
    let expected = format!("v{limit}");
    assert_eq!(item.value(), Value::Bytes(expected.as_bytes()));
}

/// The retry is still BOUNDED. A hook that republishes on every revalidation
/// never lets the reader win, and the get must give up — quickly, and by
/// spending exactly `REVALIDATE_RETRIES` mismatches.
///
/// This is the guard on the rejected alternative: an unbounded retry passes
/// every other test here, and hangs this one. It runs on its own thread under
/// a watchdog so that failure shows up as a wedge report rather than a CI job
/// burning its 30-minute cap.
#[test]
fn bounded_giveup_when_every_revalidation_loses() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");

    let fired = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let cache = Arc::clone(&cache);
        let fired = Arc::clone(&fired);
        std::thread::spawn(move || {
            // The hooks are thread-local, so they must be installed on the
            // thread that runs the get.
            let hook =
                revalidation_fault::on_before_revalidate(republisher(&cache, &fired, usize::MAX));
            let missed = cache.get(KEY).is_none();
            drop(hook);
            tx.send(missed).expect("watchdog receiver must outlive us");
        })
    };

    let missed = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(missed) => missed,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "get_pinned wedged: the revalidation retry must stay BOUNDED — lock-free \
             is not starvation-free, and nothing bounds how long writers keep \
             rewriting a hot key"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Ok(()) => unreachable!("the worker sends before it exits"),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    };
    worker.join().expect("worker must not panic");

    assert!(
        missed,
        "a reader that never once wins the revalidation has nothing sound to hand \
         out: the pinned bytes were not the published item"
    );
    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        REVALIDATE_RETRIES,
        "give-up must cost exactly the budget: no more (a wider bound is a latency \
         cliff) and no fewer (a tighter one is #65 again)"
    );
    assert!(
        cache.get(KEY).is_some(),
        "the key must still be live once the churn stops — the give-up is a bounded \
         concession, not a removal"
    );
}

// ── The bounded stale-incarnation arm (#50) ───────────────────────────────
//
// `relookup_after_pin_failure` has a second arm: when the failed pin's
// location names an incarnation that is GONE (`segments.resolve` says `None`),
// the retry is charged against `REVALIDATE_RETRIES` and gives up when the
// budget runs out. The safety argument is that a charge costs a real segment
// RECYCLE, so exhausting the budget takes ~16 full segment lifecycles inside
// one lookup -> pin window — implausible for a live key.
//
// Two things make that argument testable rather than merely assertable:
//
//   * the churn must actually RECYCLE, not just republish. Republication alone
//     leaves the old location resolvable, so the arm is never entered and a
//     test of it passes vacuously — the same trap as a `get` benchmark that
//     never inserts.
//   * the test must observe that the arm was entered. That is
//     `stale_incarnation_charges`, a counter inside the arm itself, so a
//     future change that stops routing stale locations here fails the test
//     instead of quietly turning it into a no-op.

use crate::segcache::stale_incarnation_charges;
use crate::segments::ClearOutcome;
use core::num::NonZeroU32;

/// Fixed-width so every republication is byte-identical in size, and a
/// one-item segment fits each of them exactly.
fn churn_value(n: usize) -> String {
    format!("v{n:03}")
}

/// A heap of segments that hold **exactly one item**, so a republication
/// necessarily lands in a different segment and seals the one it left. That is
/// what lets the hook recycle the just-resolved location's segment without
/// touching the copy the reader is meant to find.
fn one_item_per_segment_cache() -> Arc<Segcache> {
    let probe = churn_value(0);
    let item_size = keyvalue::item_size(KEY.len(), &Value::Bytes(probe.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size) as i32;
    Arc::new(
        Segcache::builder()
            .segment_size(segment_size)
            // Room to spare: the churn keeps only two segments in service at a
            // time, so nothing ever evicts and every relocation below is one
            // the hook performed on purpose.
            .heap_size(segment_size as usize * 16)
            .hash_power(16)
            .build()
            .expect("failed to create cache"),
    )
}

/// Stand-in for a concurrent writer AND a concurrent evictor, fired in the
/// lookup -> pin window: republish `KEY` into a fresh segment and see the
/// segment the reader just resolved RECYCLED before returning.
///
/// The recycle is the whole point. It is what bumps the segment's generation,
/// which is what makes the reader's location a stale incarnation, which is
/// what charges the budget. With one item per segment the republish performs
/// it itself — retiring the superseded copy empties the old segment, and
/// `remove_at` frees an emptied segment on the spot — so this is production
/// machinery, not a hand-driven drain. The hook asserts the generation really
/// moved, so a future change that stops recycling here fails loudly rather
/// than leaving the test to pass without ever entering the arm under test.
fn republish_and_recycle(
    cache: &Arc<Segcache>,
    fired: &Arc<AtomicUsize>,
    limit: usize,
) -> impl Fn() + 'static {
    let cache = Arc::clone(cache);
    let fired = Arc::clone(fired);
    move || {
        let n = fired.load(AtomicOrdering::Relaxed);
        if n >= limit {
            return;
        }
        fired.store(n + 1, AtomicOrdering::Relaxed);

        // Single-threaded, so this is precisely the location the reader's
        // lookup just returned and is about to try to pin.
        let old = location_of(&cache, KEY).expect("the key is live throughout this test");
        let old_seg = NonZeroU32::new(unpack_location(old).0)
            .expect("a published location always names a segment");
        let generation_before = cache.segments.generation(old_seg);

        cache
            .insert(KEY, churn_value(n + 1).as_bytes(), None, TTL)
            .expect("republish must succeed: the cache is far from full");
        let new = location_of(&cache, KEY).expect("the republish must publish somewhere");
        assert_ne!(
            unpack_location(new).0,
            old_seg.get(),
            "one item per segment: the republish must move the key OUT of the \
             segment this hook is about to recycle"
        );

        // The republish normally recycles the old segment on its own: the
        // `remove_at` that retires the superseded copy frees a segment the
        // moment its last live item goes away, and one item per segment means
        // that is exactly now. Drive the drain by hand if some future change
        // takes that path away — what this hook owes the test is that the
        // incarnation ENDS, not which path ends it.
        if cache.segments.generation(old_seg) == generation_before {
            assert!(
                cache.segments.claim_for_drain_for_test(old_seg),
                "the sealed, superseded segment must be claimable"
            );
            assert_eq!(
                cache
                    .segments
                    .finalize_drained_for_test(old_seg, &cache.hashtable),
                ClearOutcome::Freed,
                "nothing pins the superseded segment, so it must be recycled \
                 here and not merely condemned"
            );
        }
        assert_ne!(
            cache.segments.generation(old_seg),
            generation_before,
            "NO RECYCLE, NO CHARGE: the stale-incarnation arm is only reachable \
             once the segment's generation has actually advanced"
        );
    }
}

/// **The stale-incarnation budget, exercised.** A key that is never deleted,
/// republished and whose previous segment is fully recycled on every
/// from-scratch lookup, `REVALIDATE_RETRIES - 1` times inside a single `get`.
///
/// Two properties, and the second is what stops the first from being empty:
///
///   * **no false absent.** The key is live and resolvable at every instant,
///     so the `get` must hand back the current value. Fifteen consecutive
///     recycles inside one lookup -> pin window is far past anything a live
///     key can encounter; the budget must absorb them.
///   * **the budget was really charged.** `stale_incarnation_charges` counts
///     entries into the arm. Without this the test would still pass if the
///     churn stopped producing stale incarnations, or if the arm stopped
///     being routed to — exactly the decay the arm's untested safety argument
///     is exposed to.
#[test]
fn budget_absorbs_recycled_incarnations_without_a_false_absent() {
    let cache = one_item_per_segment_cache();
    cache
        .insert(KEY, churn_value(0).as_bytes(), None, TTL)
        .expect("seed");
    // Discard anything an earlier test on this thread left behind.
    let _ = stale_incarnation_charges::take();

    let fired = Arc::new(AtomicUsize::new(0));
    let limit = REVALIDATE_RETRIES - 1;
    let item = {
        let _hook =
            revalidation_fault::on_after_lookup(republish_and_recycle(&cache, &fired, limit));
        cache.get(KEY)
    };
    let charged = stale_incarnation_charges::take();
    eprintln!(
        "stale-incarnation arm: {charged} of {REVALIDATE_RETRIES} budget attempts charged \
         ({} republish+recycle cycles inside one get)",
        fired.load(AtomicOrdering::Relaxed)
    );

    // The property first, so a regression reports the user-visible failure
    // rather than a bookkeeping mismatch downstream of it.
    let item = item.expect(
        "false absent: the key was republished and recycled, never deleted, and \
         resolved at every instant — the bounded stale-incarnation retry must \
         not turn that into a miss",
    );
    let expected = churn_value(limit);
    assert_eq!(
        item.value(),
        Value::Bytes(expected.as_bytes()),
        "the item handed out must be the one the surviving location publishes"
    );

    // Then the anti-vacuity guard: the property above is only worth anything
    // if the arm it is about was actually taken.
    assert!(
        charged > 0,
        "VACUOUS: the get never entered the stale-incarnation arm, so this test \
         asserted nothing about the budget it is supposed to bound"
    );
    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        limit,
        "the hook must have run its full quota of republish+recycle cycles"
    );
    assert_eq!(
        charged, limit,
        "every recycled incarnation must cost exactly one budget attempt: fewer \
         means the churn stopped invalidating locations, more means some other \
         path is spending the read budget"
    );
}
