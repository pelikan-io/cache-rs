// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Concurrency tests for numeric in-place updates (`wrapping_add`/
//! `saturating_sub`) racing CAS-style replacement.
//!
//! The invariant under test: **every acked increment is observable**. An
//! `incr` that returned `Ok` must be reflected in the key's final value
//! unless a LEGAL later write replaced it — and a `cas` that only ever
//! writes back the exact value it read (under the token it read) can
//! never be such a write: its token folds the numeric item's seqlock
//! version, so a successful cas proves no increment landed between its
//! read and its publish, making it value-preserving.
//!
//! Two broken interleavings previously violated this:
//!
//! - cas verified the FULL token (location + generation + numeric
//!   version) at token-check time, but published via a LOCATION-ONLY
//!   hashtable slot CAS. An in-place increment changes neither location
//!   nor slot bits, so an incr landing in the token-check -> publish
//!   window (which spans `reserve_and_define`) was invisible to the
//!   publish: cas returned Ok and overwrote the acked increment.
//! - the mirror direction: `numeric_update` re-validated
//!   `lookup(key) == location` BEFORE its seqlocked write, so a cas
//!   publishing in between left the incr writing to the unlinked old
//!   item — an acked increment that never became visible.
//!
//! These tests drive both windows with real racing threads. They are
//! probabilistic but heavily biased: tens of thousands of cas windows
//! against a continuously incrementing sibling thread hit the races
//! reliably before the fix.

use crate::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Wait (bounded) for a worker thread: `Ok` on its completion signal,
/// panic with `name` on a wedge (timeout), and propagate the worker's own
/// panic if it died before signalling. Same shape as
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

/// A cache comfortably larger than the garbage the cas churn below
/// produces, so eviction never runs and the counter is never relocated
/// by anything but the cas publishes themselves.
fn roomy_cache() -> Segcache {
    Segcache::builder()
        .segment_size(1024 * 1024)
        .heap_size(64 * 1024 * 1024)
        .hash_power(16)
        .build()
        .expect("failed to create cache")
}

/// Value-preserving cas racing incr: thread A loops get -> cas writing
/// back the EXACT value it read under the token it read; thread B loops
/// incr, counting acks. A successful value-preserving cas cannot change
/// the count it observed, so at quiescence:
///
///   final == initial + acked_increments
///
/// A false STORED (cas succeeding on a token whose numeric version went
/// stale) overwrites acked increments and drives `final` short; an incr
/// writing to an already-unlinked item (the mirror window) goes missing
/// the same way. Either bug trips the assertion.
fn cas_vs_incr_lost_ack(cas_threads: usize, incr_threads: usize, cas_iters: usize) {
    const KEY: &[u8] = b"cnt";
    let ttl = Duration::from_secs(3600);

    let cache = Arc::new(roomy_cache());
    cache.insert(KEY, 0u64, None, ttl).expect("seed counter");

    let stop = Arc::new(AtomicBool::new(false));
    let acked = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..incr_threads {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        let acked = Arc::clone(&acked);
        handles.push(std::thread::spawn(move || {
            while !stop.load(AtomicOrdering::Relaxed) {
                match cache.wrapping_add(KEY, 1) {
                    Ok(_) => {
                        acked.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                    Err(e) => panic!("incr must not fail in this test: {e:?}"),
                }
            }
        }));
    }
    for _ in 0..cas_threads {
        let cache = Arc::clone(&cache);
        handles.push(std::thread::spawn(move || {
            for _ in 0..cas_iters {
                let Some(item) = cache.get(KEY) else {
                    panic!("counter must stay resident (no eviction pressure)");
                };
                let token = item.cas();
                let Value::U64(v) = item.value() else {
                    panic!("counter must stay numeric");
                };
                drop(item);
                // Write back exactly what was read: a successful cas is
                // then value-preserving by the token's version-folding
                // guarantee. Exists/NotFound are legal race outcomes.
                let _ = cache.cas(KEY, v, None, ttl, token);
            }
        }));
    }

    // Watchdog wrapper: join the workers from a supervisor thread that
    // signals completion, so a wedge fails the test instead of hanging CI.
    let supervisor = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            // cas threads are the finite ones; join them, then stop incrs.
            let mut incrs = Vec::new();
            for (i, h) in handles.into_iter().enumerate() {
                if i < incr_threads {
                    incrs.push(h);
                } else {
                    h.join().expect("cas worker panicked");
                }
            }
            stop.store(true, AtomicOrdering::Relaxed);
            for h in incrs {
                h.join().expect("incr worker panicked");
            }
            tx.send(()).ok();
        }
    });
    join_within("cas-vs-incr workers", rx, supervisor, 120);

    let final_item = cache.get(KEY).expect("counter resident at quiescence");
    let Value::U64(final_value) = final_item.value() else {
        panic!("counter must stay numeric");
    };
    let acked = acked.load(AtomicOrdering::Relaxed);
    assert!(acked > 0, "test must exercise increments");
    assert_eq!(
        final_value, acked,
        "acked increments destroyed: final {final_value} != acked {acked} \
         (every acked incr must be observable; value-preserving cas cannot \
         legally absorb any)"
    );
}

/// Single cas thread vs single incr thread — the narrowest pairing of the
/// two windows.
#[test]
fn cas_racing_incr_never_destroys_acked_increment() {
    cas_vs_incr_lost_ack(1, 1, 20_000);
}

/// Tight-loop stress variant: multiple cas and incr threads on one key.
/// Successful cas ops remain value-preserving, so the same exact-count
/// invariant holds.
#[test]
fn cas_incr_stress_exact_accounting() {
    cas_vs_incr_lost_ack(2, 2, 15_000);
}

/// Engine-level concurrent-numerics totals: N threads x M `wrapping_add`
/// through the full `Segcache` path (lookup, expiry check, reader pin,
/// re-validation, seqlocked RMW) on one key must sum exactly — validates
/// the keyvalue atomic-numerics contract end to end.
#[test]
fn concurrent_wrapping_add_totals_exact() {
    const KEY: &[u8] = b"total";
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 25_000;
    let ttl = Duration::from_secs(3600);

    let cache = Arc::new(roomy_cache());
    cache.insert(KEY, 0u64, None, ttl).expect("seed counter");

    let (tx, rx) = mpsc::channel();
    let supervisor = std::thread::spawn({
        let cache = Arc::clone(&cache);
        move || {
            let mut handles = Vec::new();
            for _ in 0..THREADS {
                let cache = Arc::clone(&cache);
                handles.push(std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        cache
                            .wrapping_add(KEY, 1)
                            .expect("incr on resident counter");
                    }
                }));
            }
            for h in handles {
                h.join().expect("incr worker panicked");
            }
            tx.send(()).ok();
        }
    });
    join_within("wrapping_add workers", rx, supervisor, 120);

    let item = cache.get(KEY).expect("counter resident");
    let Value::U64(v) = item.value() else {
        panic!("counter must stay numeric");
    };
    assert_eq!(v, THREADS * PER_THREAD, "lost concurrent increments");
}
