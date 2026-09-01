// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Concurrency tests for in-place numeric updates racing merge RELOCATION —
//! the overlap both existing suites excluded by design
//! (`numeric_concurrency_tests` uses a roomy cache so eviction never runs;
//! `pin_failure_tests` never runs numeric ops).
//!
//! The invariant under test: **relocation is value/version-coherent for
//! numeric items**. `copy_into` (merge) and `s3fifo_promote_from` move an
//! item with a raw byte copy and then relink it via a hashtable
//! location-CAS. The drain claim waits on `active_writers` and
//! `active_removers` but NOT on readers — and `numeric_update` mutates item
//! bytes holding only a READER pin plus the item's seqlock writer lock. So
//! unless relocation itself participates in that per-item lock, an incr can
//! interleave with the copy:
//!
//! - the copy reads the value word before the incr's store but relinks
//!   after the incr's in-lock linkage check passed — the published
//!   destination misses an ACKED increment (the exact symptom the cas
//!   publish gate closed for cas-vs-incr);
//! - the copy captures the version word while the incr holds the lock
//!   (odd) — the destination is published permanently "write in
//!   progress", and every subsequent seqlock read (`get`'s `value()`,
//!   the next incr's lock acquire) spins forever: a wedge;
//! - the unsynchronized read/write overlap is a torn value/CRC pair and
//!   formally a data race.
//!
//! The test drives a hot counter (single incr thread, so every ack has an
//! exact expected value) against continuous merge churn. Assertions:
//! (a) every `wrapping_add` ack returns exactly `previous + 1` — a stale
//!     relocated value trips it immediately;
//! (b) no wedge — every op completes (watchdog);
//! (c) `check_integrity` stays clean at quiescence (debug feature).

use crate::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const KEY_LEN: usize = 7;
const VAL_LEN: usize = 7;
const COUNTER_KEY: &[u8] = b"cnt0000";

/// Small Merge-policy cache (same shape as `pin_failure_tests`): segments
/// sized for `ITEMS_PER_SEGMENT` filler items, so churn inserts drive
/// continuous merge eviction and the hot counter is relocated many times
/// per run.
fn small_merge_cache(total_segments: usize) -> Segcache {
    let sample = "V000000";
    assert_eq!(sample.len(), VAL_LEN);
    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache")
}

/// Wait (bounded) for a worker thread — same shape as
/// `pin_failure_tests::join_within`.
fn join_within(name: &str, rx: mpsc::Receiver<()>, handle: std::thread::JoinHandle<()>, secs: u64) {
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(()) => match handle.join() {
            Ok(()) => {}
            Err(payload) => std::panic::resume_unwind(payload),
        },
        Err(mpsc::RecvTimeoutError::Disconnected) => match handle.join() {
            Ok(()) => panic!("{name} exited without signalling completion"),
            Err(payload) => std::panic::resume_unwind(payload),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{name} wedged: did not complete within {secs}s")
        }
    }
}

/// Hot numeric counter vs continuous merge relocation.
///
/// One incr thread hammers `wrapping_add(COUNTER_KEY, 1)`, tracking the
/// exact expected value: with a single writer, every ack MUST return
/// `previous + 1` — relocation is value-preserving, so only a lost acked
/// increment (incr applied to an orphaned pre-relocation item, or a
/// relocation publishing a pre-increment copy) can break the sequence. A
/// genuine eviction of the counter surfaces as `NotFound` (the entry is
/// unlinked first), which the thread handles by reseeding — it can never be
/// confused with a lost ack. The counter's frequency is bumped on every
/// incr lookup and preserved across relinks, so prune retains it and
/// evictions stay rare-to-absent.
///
/// A relocation-published ODD version word instead wedges the next incr /
/// get in an unbounded seqlock spin — caught by the watchdog.
#[test]
fn numeric_ops_survive_merge_relocation() {
    /// Wall-clock churn budget: the race window (an incr's lock-held
    /// critical section overlapping one relocation's copy->relink span) is
    /// tens of nanoseconds wide but retried ~10k times per second of
    /// churn, so a few seconds hits it reliably pre-fix.
    const CHURN_SECS: u64 = 4;

    let cache = Arc::new(small_merge_cache(16));
    let ttl = Duration::from_secs(3600);
    let stop = Arc::new(AtomicBool::new(false));
    let acked = Arc::new(AtomicU64::new(0));
    let final_expected = Arc::new(AtomicU64::new(0));
    let reseeds = Arc::new(AtomicU64::new(0));
    let moves = Arc::new(AtomicU64::new(0));

    cache
        .insert(COUNTER_KEY, 0u64, None, ttl)
        .expect("seed counter");

    // Churn: unique filler keys force continuous merge eviction, which
    // relocates the (high-frequency, retained) counter via copy_into.
    let (ctx, crx) = mpsc::channel();
    let churner = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(CHURN_SECS);
            let mut i: u64 = 0;
            while std::time::Instant::now() < deadline {
                let key = format!("c{i:08}");
                i += 1;
                // Reserve failure under pressure is legal; retry briefly.
                for _ in 0..100 {
                    if cache.insert(key.as_bytes(), b"Vchurn0", None, ttl).is_ok() {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
            stop.store(true, AtomicOrdering::Release);
            let _ = ctx.send(());
        })
    };

    // Single incr writer: exact per-ack accounting.
    let (itx, irx) = mpsc::channel();
    let incrementer = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        let acked = Arc::clone(&acked);
        let final_expected = Arc::clone(&final_expected);
        let reseeds = Arc::clone(&reseeds);
        std::thread::spawn(move || {
            let mut expected: u64 = 0;
            while !stop.load(AtomicOrdering::Acquire) {
                match cache.wrapping_add(COUNTER_KEY, 1) {
                    Ok(v) => {
                        expected += 1;
                        acked.fetch_add(1, AtomicOrdering::Relaxed);
                        assert_eq!(
                            v, expected,
                            "acked increment lost across a merge relocation: \
                             incr returned {v}, expected {expected} (single \
                             writer; relocation must be value-preserving)"
                        );
                    }
                    Err(SegcacheError::NotFound) => {
                        // Genuine eviction (entry unlinked before the incr's
                        // lookup) — reseed and restart the expected sequence.
                        reseeds.fetch_add(1, AtomicOrdering::Relaxed);
                        expected = 0;
                        for _ in 0..1000 {
                            if cache.insert(COUNTER_KEY, 0u64, None, ttl).is_ok() {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                    }
                    Err(e) => panic!("unexpected incr failure: {e:?}"),
                }
            }
            // Publish the final expected value for the quiescent check.
            final_expected.store(expected, AtomicOrdering::Release);
            let _ = itx.send(());
        })
    };

    // Monitor: count the counter's location changes, proving the test
    // actually exercises relocation (a merge that never moves the counter
    // tests nothing).
    let (mtx, mrx) = mpsc::channel();
    let monitor = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        let moves = Arc::clone(&moves);
        std::thread::spawn(move || {
            let mut last: Option<Location> = None;
            while !stop.load(AtomicOrdering::Acquire) {
                let verifier = cache.segments.verifier();
                match cache
                    .hashtable
                    .lookup_no_freq_update(COUNTER_KEY, &verifier)
                {
                    Some((loc, _)) => {
                        if let Some(prev) = last {
                            if prev != loc {
                                moves.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                        }
                        last = Some(loc);
                    }
                    None => last = None,
                }
                std::thread::yield_now();
            }
            let _ = mtx.send(());
        })
    };

    join_within("churn writer", crx, churner, 300);
    join_within("counter incrementer", irx, incrementer, 60);
    join_within("location monitor", mrx, monitor, 60);

    eprintln!(
        "numeric_ops_survive_merge_relocation: acked={} reseeds={} moves={}",
        acked.load(AtomicOrdering::Acquire),
        reseeds.load(AtomicOrdering::Relaxed),
        moves.load(AtomicOrdering::Relaxed)
    );

    // Quiescent checks: the resident value matches the single writer's
    // expectation; a post-storm numeric op and get complete (no wedged
    // seqlock); relocation actually happened.
    let expected = final_expected.load(AtomicOrdering::Acquire);
    if let Some(item) = cache.get(COUNTER_KEY) {
        let Value::U64(v) = item.value() else {
            panic!("counter must stay numeric");
        };
        assert_eq!(
            v, expected,
            "final counter value diverged from acked increments"
        );
        drop(item);
        assert_eq!(
            cache.wrapping_add(COUNTER_KEY, 1),
            Ok(expected + 1),
            "post-storm incr must complete and see the acked total"
        );
    }
    assert!(
        moves.load(AtomicOrdering::Relaxed) > 0,
        "counter was never relocated: test failed to exercise merge relocation \
         (tune churn/segment sizing)"
    );
    assert!(expected > 0 || reseeds.load(AtomicOrdering::Relaxed) > 0);

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("integrity clean after relocation churn");
}
