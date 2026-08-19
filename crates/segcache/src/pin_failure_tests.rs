// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Tests for the segment-pin FAILURE paths of the public operations: what
//! `get`/`delete`/`insert`/`cas` must do when `acquire_item_at` (reader pin)
//! or `try_pin_remover` fails because a drain owns the segment.
//!
//! Under the default Merge eviction policy a drain does NOT imply the
//! segment's live items are going away: merge eviction drains a candidate
//! while RETAINING its live items (they are relocated into the copy
//! destination and republished via `cas_location`). The failure paths must
//! therefore never treat "segment unreadable/unpinnable" as "item gone":
//!
//! - Bug 1: `get` returning `None` on a reader-pin failure is a FALSE MISS —
//!   the key reappears once the merge republishes it (breaking
//!   read-your-writes, and corrupting `add`/`replace` built on top).
//! - Bug 2: `delete` acking `true` on a remover-pin failure WITHOUT unlinking
//!   the hashtable entry lets a merge drain relocate the item — an acked
//!   delete that RESURRECTS.
//! - Bug 3: `insert`/`cas` spinning on a remover-pin failure while holding
//!   the new reservation's `WriterPin` deadlocks when the old item lives in
//!   the SAME segment as the reservation: the drain waits for
//!   `active_writers == 0` (our pin) while we wait for the drain to sweep
//!   the old entry — two threads wedged at 100% CPU.
//!
//! The deterministic tests below drive the drain protocol directly through
//! the test-only `claim_for_drain_for_test` shim (a claimed segment with the
//! relocation "in flight" is exactly what an in-progress merge looks like to
//! the public ops). The stress test exercises the same windows through real
//! merge eviction churn.
//!
//! NOTE on loom: the replace-vs-drain deadlock (bug 3) is a protocol-level
//! cycle through `TtlBucket::chain_lock` (a `std::sync::Mutex`, deliberately
//! not loom-instrumented), the `Backoff` spin loops, and the full
//! reserve/publish path. loom can only model loom-instrumented primitives
//! and bounded executions, so a faithful model would require rebuilding the
//! whole insert/drain protocol on loom types — intractable state space. The
//! watchdogged wedge test below covers it instead.

use crate::*;
use core::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const KEY_LEN: usize = 7;
const VAL_LEN: usize = 7;

/// Build a small Merge-policy cache: `total_segments` segments sized to hold
/// exactly `ITEMS_PER_SEGMENT` items of `KEY_LEN`/`VAL_LEN` each. Merge is
/// the default production policy and the one whose drains retain live items.
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

/// Insert `key` and then filler keys until `key`'s segment seals (the tail
/// advances past it), returning the key's location and segment id. A Sealed
/// segment is exactly what a merge drain claims.
fn insert_and_seal(cache: &Segcache, key: &[u8], val: &[u8]) -> (Location, NonZeroU32) {
    assert_eq!(key.len(), KEY_LEN);
    assert_eq!(val.len(), VAL_LEN);
    let ttl = Duration::from_secs(3600);
    cache.insert(key, val, None, ttl).expect("insert target");

    let verifier = cache.segments.verifier();
    let (location, _freq) = cache
        .hashtable
        .lookup_no_freq_update(key, &verifier)
        .expect("target must resolve");
    let (seg_raw, _offset) = unpack_location(location);
    let seg_id = NonZeroU32::new(seg_raw).expect("target location must be a real segment");

    for i in 0..(2 * ITEMS_PER_SEGMENT) {
        if cache.segments.header(seg_id).state() == State::Sealed {
            break;
        }
        let filler = format!("f{i:06}");
        cache
            .insert(filler.as_bytes(), val, None, ttl)
            .expect("filler insert");
    }
    assert_eq!(
        cache.segments.header(seg_id).state(),
        State::Sealed,
        "target's segment must seal"
    );

    // The fill is far below eviction pressure, so the target must not have
    // moved.
    let (loc_after, _) = cache
        .hashtable
        .lookup_no_freq_update(key, &verifier)
        .expect("target still resolves");
    assert_eq!(loc_after, location, "target must not relocate during fill");

    (location, seg_id)
}

/// Wait (bounded) for a worker thread: `Ok` on its completion signal, panic
/// with `name` on a wedge (timeout), and propagate the worker's own panic if
/// it died before signalling. Keeps a wedged run a test FAILURE rather than
/// a CI hang — the wedged threads leak, but the process exits with the
/// harness.
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

/// Bug 1 (false absence during merge drains): a `get` that races a drain of
/// the key's segment must retry until the drain resolves — never report a
/// LIVE key as missing. Here the drain resolves by the revert arc
/// (Draining -> Sealed, the merge's found-pinned revert) with the item
/// untouched; the get must then return it.
#[test]
fn get_retries_through_transient_drain_instead_of_false_miss() {
    let cache = Arc::new(small_merge_cache(8));
    let (_location, seg_id) = insert_and_seal(&cache, b"target0", b"Vtarge0");

    // A merge drain claims the segment (Sealed -> Draining) and is now "mid
    // copy": the item is live, published, but its segment is unreadable.
    assert!(cache.segments_for_test().claim_for_drain_for_test(seg_id));

    let (tx, rx) = mpsc::channel();
    let reader = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let got = cache.get(b"target0").map(|item| match item.value() {
                Value::Bytes(b) => b.to_vec(),
                Value::U64(v) => v.to_be_bytes().to_vec(),
            });
            let _ = tx.send(got);
        })
    };

    // Give the reader time to hit the unreadable segment. (Pre-fix it
    // returned a false None here instantly.)
    std::thread::sleep(Duration::from_millis(50));

    // The drain finishes without touching the item: revert Draining -> Sealed.
    assert!(
        cache.segments.header(seg_id).cas_metadata(
            State::Draining,
            State::Sealed,
            None,
            None,
            crate::sync::Ordering::SeqCst,
        ),
        "test owns the claimed segment; revert must succeed"
    );

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("get wedged after the segment recovered from the drain");
    assert_eq!(
        got.as_deref(),
        Some(&b"Vtarge0"[..]),
        "live key read as missing while its segment drained (false miss)"
    );
    let _ = reader.join();
}

/// Bug 1 termination guard: when the key is genuinely GONE (the drain's
/// hashtable sweep removed it), the retrying `get` must still terminate
/// promptly with `None` — the fresh lookup itself returns nothing.
#[test]
fn get_terminates_when_key_removed_during_drain() {
    let cache = Arc::new(small_merge_cache(8));
    let (location, seg_id) = insert_and_seal(&cache, b"target1", b"Vtarge1");

    assert!(cache.segments_for_test().claim_for_drain_for_test(seg_id));
    // The drain sweeps the entry (what `Segment::clear` does per item).
    assert!(cache.hashtable.remove(b"target1", location));

    let (tx, rx) = mpsc::channel();
    let reader = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let got = cache.get(b"target1").map(|_| ());
            let _ = tx.send(got.is_some());
        })
    };

    let found = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("get wedged on a genuinely-removed key (retry must terminate)");
    assert!(!found, "removed key must read as missing");
    let _ = reader.join();

    // Cleanup so the claimed segment is not left Draining.
    assert!(cache.segments.header(seg_id).cas_metadata(
        State::Draining,
        State::Sealed,
        None,
        None,
        crate::sync::Ordering::SeqCst,
    ));
}

/// Bug 2 (acked delete resurrected by merge relocation): a `delete` that
/// races a drain of the key's segment may ack `true` ONLY if the hashtable
/// entry is actually gone. A merge drain relocates every item still present
/// in the hashtable (`copy_into`'s `get_item_frequency` gate), so an acked
/// delete that leaves the entry behind resurrects — a hard memcached
/// contract violation.
#[test]
fn acked_delete_during_drain_unlinks_the_entry() {
    let cache = small_merge_cache(8);
    let (location, seg_id) = insert_and_seal(&cache, b"victim0", b"Vvicti0");

    // A merge drain claims the segment and is "mid copy".
    assert!(cache.segments_for_test().claim_for_drain_for_test(seg_id));

    // DELETE while the drain is in flight: the key is live, so it acks.
    assert!(
        cache.delete(b"victim0"),
        "delete of a live key must be acked"
    );

    // The ack must be real: the entry must be unlinked, because this is
    // exactly what the merge's relocation gate consults. A left-behind
    // entry would be relocated (resurrecting the acked delete).
    assert!(
        cache
            .hashtable
            .get_item_frequency(b"victim0", location)
            .is_none(),
        "acked delete left the hashtable entry; a merge drain would relocate (resurrect) it"
    );

    // Drain finishes via the revert arc; the key must stay deleted.
    assert!(cache.segments.header(seg_id).cas_metadata(
        State::Draining,
        State::Sealed,
        None,
        None,
        crate::sync::Ordering::SeqCst,
    ));
    assert!(
        cache.get(b"victim0").is_none(),
        "acked delete resurrected after the drain"
    );
}

/// Bug 2, `Relinking` variant: a merge/promotion copy DESTINATION mid-fill
/// also fails the remover pin, and NO drain will ever sweep its entries (the
/// destination is never drained by its owner). An acked delete must unlink
/// the entry itself or the key simply stays live forever.
#[test]
fn acked_delete_in_relinking_segment_unlinks_the_entry() {
    let cache = small_merge_cache(8);
    let (location, seg_id) = insert_and_seal(&cache, b"victim1", b"Vvicti1");

    // Force the copy-destination state (what `link_dest_at_head` publishes
    // while the owner fills the destination).
    assert!(cache.segments.header(seg_id).cas_metadata(
        State::Sealed,
        State::Relinking,
        None,
        None,
        crate::sync::Ordering::SeqCst,
    ));

    assert!(
        cache.delete(b"victim1"),
        "delete of a live key must be acked"
    );
    assert!(
        cache
            .hashtable
            .get_item_frequency(b"victim1", location)
            .is_none(),
        "acked delete left the hashtable entry in a Relinking segment (nobody sweeps it)"
    );

    // Fill completes (Relinking -> Sealed); the key must stay deleted.
    assert!(cache.segments.header(seg_id).cas_metadata(
        State::Relinking,
        State::Sealed,
        None,
        None,
        crate::sync::Ordering::SeqCst,
    ));
    assert!(
        cache.get(b"victim1").is_none(),
        "acked delete resurrected after the fill completed"
    );
}

/// Bug 3 (replace-vs-drain deadlock), `insert` path: one thread re-setting
/// the SAME key (old value + new reservation co-locate in the Live tail)
/// races a thread draining the bucket (`clear`, same claim as flush_all /
/// lazy expiry / eviction of a just-sealed tail). Pre-fix: the drain claims
/// the tail and waits for `active_writers == 0` (the writer's own
/// reservation pin) while the writer spins on `try_pin_remover` failure
/// re-finding the old entry the blocked drain can never sweep — both wedge.
/// This is the SAME-KEY variant `concurrent_reservers_vs_drain_same_bucket`
/// misses (it uses unique keys, so its writers never take the replace arm).
#[test]
fn same_key_replace_vs_drain_completes() {
    const SETS: usize = 20_000;

    let cache = Arc::new(small_merge_cache(8));
    let ttl = Duration::from_secs(3600);
    let stop = Arc::new(AtomicBool::new(false));

    let (wtx, wrx) = mpsc::channel();
    let writer = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for i in 0..SETS {
                let val = format!("W{:06}", i % 1_000_000);
                // Tolerate transient reserve failure (the drainer churns the
                // pool), but never total starvation.
                let mut ok = false;
                for _ in 0..1000 {
                    if cache.insert(b"hotkey0", val.as_bytes(), None, ttl).is_ok() {
                        ok = true;
                        break;
                    }
                    std::hint::spin_loop();
                }
                assert!(ok, "insert starved during drain churn");
            }
            stop.store(true, AtomicOrdering::Release);
            let _ = wtx.send(());
        })
    };

    let (dtx, drx) = mpsc::channel();
    let drainer = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(AtomicOrdering::Acquire) {
                let _ = cache.clear();
            }
            let _ = dtx.send(());
        })
    };

    join_within(
        "same-key writer (replace-vs-drain deadlock)",
        wrx,
        writer,
        120,
    );
    join_within("bucket drainer", drx, drainer, 30);

    // The engine is still fully functional afterwards.
    cache
        .insert(b"hotkey0", b"Wfinal0", None, ttl)
        .expect("post-storm insert");
    let item = cache.get(b"hotkey0").expect("post-storm get");
    assert_eq!(item.value(), Value::Bytes(b"Wfinal0"));
}

/// Bug 3, `cas`/`replace_at` path: same wedge through `replace_at`'s
/// pin-failure spin (`cas` reserves in the tail where the old value of the
/// same key also lives, then spins on the drain that is waiting on its pin).
#[test]
fn same_key_cas_vs_drain_completes() {
    const OPS: usize = 20_000;

    let cache = Arc::new(small_merge_cache(8));
    let ttl = Duration::from_secs(3600);
    let stop = Arc::new(AtomicBool::new(false));

    let (wtx, wrx) = mpsc::channel();
    let writer = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for i in 0..OPS {
                let val = format!("C{:06}", i % 1_000_000);
                let cas = match cache.get_no_freq_incr(b"hotkey1") {
                    Some(item) => item.cas(),
                    None => {
                        // Key drained away — reinstall it and move on.
                        let _ = cache.insert(b"hotkey1", val.as_bytes(), None, ttl);
                        continue;
                    }
                };
                // Any outcome is legal under concurrent drains (Ok, Exists,
                // NotFound, transient reserve failure); the property under
                // test is completion.
                let _ = cache.cas(b"hotkey1", val.as_bytes(), None, ttl, cas);
            }
            stop.store(true, AtomicOrdering::Release);
            let _ = wtx.send(());
        })
    };

    let (dtx, drx) = mpsc::channel();
    let drainer = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(AtomicOrdering::Acquire) {
                let _ = cache.clear();
            }
            let _ = dtx.send(());
        })
    };

    join_within(
        "same-key cas writer (replace-vs-drain deadlock)",
        wrx,
        writer,
        120,
    );
    join_within("bucket drainer", drx, drainer, 30);

    cache
        .insert(b"hotkey1", b"Cfinal0", None, ttl)
        .expect("post-storm insert");
    let item = cache.get(b"hotkey1").expect("post-storm get");
    assert_eq!(item.value(), Value::Bytes(b"Cfinal0"));
}

/// `cas` variant of bug 1 (false absence during merge drains): `cas` mints
/// its token via `acquire_item_at(..).ok_or(NotFound)`, so a cas racing a
/// merge drain of a LIVE key returns NOT_FOUND — memcached semantics say a
/// live key can fail a cas only with EXISTS (or succeed). It must retry the
/// lookup+pin through the transient drain window, exactly as `get_pinned`
/// does. Here the drain resolves by the revert arc with the item untouched,
/// so the caller's token is still exact and the cas must succeed.
#[test]
fn cas_retries_through_transient_drain_instead_of_false_not_found() {
    let cache = Arc::new(small_merge_cache(8));
    let ttl = Duration::from_secs(3600);
    let (_location, seg_id) = insert_and_seal(&cache, b"target2", b"Vtarge2");
    let token = {
        let item = cache.get_no_freq_incr(b"target2").expect("live key");
        item.cas()
    };

    // A merge drain claims the segment (Sealed -> Draining) and is "mid
    // copy": the key is live and published, but its segment is unpinnable.
    assert!(cache.segments_for_test().claim_for_drain_for_test(seg_id));

    let (tx, rx) = mpsc::channel();
    let caser = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let res = cache.cas(b"target2", b"Wtarge2", None, ttl, token);
            let _ = tx.send(res);
        })
    };

    // Give the cas time to hit the unpinnable segment. (Pre-fix it returned
    // a false NOT_FOUND here instantly.)
    std::thread::sleep(Duration::from_millis(50));

    // The drain finishes without touching the item: revert Draining -> Sealed.
    assert!(
        cache.segments.header(seg_id).cas_metadata(
            State::Draining,
            State::Sealed,
            None,
            None,
            crate::sync::Ordering::SeqCst,
        ),
        "test owns the claimed segment; revert must succeed"
    );

    let res = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("cas wedged after the segment recovered from the drain");
    assert_ne!(
        res,
        Err(SegcacheError::NotFound),
        "cas on a LIVE key returned NOT_FOUND during a merge drain window"
    );
    assert_eq!(
        res,
        Ok(()),
        "token unchanged across the drain window; the cas must succeed"
    );
    let item = cache.get(b"target2").expect("key stays live");
    assert_eq!(item.value(), Value::Bytes(b"Wtarge2"));
    let _ = caser.join();
}

/// `try_into_numeric` variant of bug 1: same `acquire_item_at(..)
/// .ok_or(NotFound)` pattern (a #51-acknowledged follow-up), same fix — a
/// LIVE canonical-numeric key must convert, never report NOT_FOUND because
/// its segment happened to be draining.
#[test]
fn try_into_numeric_retries_through_transient_drain() {
    let cache = Arc::new(small_merge_cache(8));
    let ttl = Duration::from_secs(3600);
    let (_location, seg_id) = insert_and_seal(&cache, b"target3", b"5000000");

    assert!(cache.segments_for_test().claim_for_drain_for_test(seg_id));

    let (tx, rx) = mpsc::channel();
    let converter = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let res = cache.try_into_numeric(b"target3", 0, ttl);
            let _ = tx.send(res);
        })
    };

    std::thread::sleep(Duration::from_millis(50));

    assert!(
        cache.segments.header(seg_id).cas_metadata(
            State::Draining,
            State::Sealed,
            None,
            None,
            crate::sync::Ordering::SeqCst,
        ),
        "test owns the claimed segment; revert must succeed"
    );

    let res = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("try_into_numeric wedged after the segment recovered from the drain");
    assert_ne!(
        res,
        Err(SegcacheError::NotFound),
        "try_into_numeric on a LIVE key returned NOT_FOUND during a merge drain window"
    );
    assert_eq!(
        res,
        Ok(()),
        "conversion of a live canonical value must succeed"
    );
    let item = cache.get(b"target3").expect("key stays live");
    assert_eq!(item.value(), Value::U64(5_000_000));
    assert_eq!(
        cache.wrapping_add(b"target3", 1),
        Ok(5_000_001),
        "converted key must accept numeric ops"
    );
    let _ = converter.join();
}

/// Stress: bugs 1 and 2 through REAL merge eviction churn (no test shims).
/// A churn writer drives continuous merge eviction on a small heap; a reader
/// hammers hot keys asserting no key ever REAPPEARS after a miss (a genuine
/// eviction stays gone — only a drain-window false miss comes back); a
/// deleter asserts an acked delete never resurrects.
#[test]
fn merge_churn_no_false_miss_no_resurrection() {
    const CHURN_OPS: usize = 30_000;
    const HOT_KEYS: usize = 4;

    let cache = Arc::new(small_merge_cache(16));
    let ttl = Duration::from_secs(3600);
    let stop = Arc::new(AtomicBool::new(false));

    let hot: Vec<String> = (0..HOT_KEYS).map(|i| format!("h{i:06}")).collect();
    for k in &hot {
        cache
            .insert(k.as_bytes(), b"Vhot000", None, ttl)
            .expect("hot prefill");
    }

    // Churn: unique filler keys force continuous merge eviction (the heap is
    // 16 segments; 30k inserts turn it over many times).
    let (ctx, crx) = mpsc::channel();
    let churner = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for i in 0..CHURN_OPS {
                let key = format!("c{i:06}");
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

    // Reader: a miss on a hot key must be a GENUINE eviction (stays gone
    // until this thread itself re-inserts). Reappearance without a re-insert
    // is a drain-window false miss. Reads also bump frequency, so hot keys
    // usually survive pruning.
    let (rtx, rrx) = mpsc::channel();
    let reader = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        let hot = hot.clone();
        std::thread::spawn(move || {
            while !stop.load(AtomicOrdering::Acquire) {
                for k in &hot {
                    if cache.get(k.as_bytes()).is_some() {
                        continue;
                    }
                    // Miss: poll — a false miss reappears once the merge
                    // republishes the relocated item.
                    let mut reappeared = false;
                    for _ in 0..1000 {
                        if cache.get(k.as_bytes()).is_some() {
                            reappeared = true;
                            break;
                        }
                        std::thread::yield_now();
                    }
                    assert!(
                        !reappeared,
                        "hot key {k} reappeared after a miss: false miss during a merge drain"
                    );
                    // Genuinely evicted: reinstall (this thread owns hot keys).
                    let _ = cache.insert(k.as_bytes(), b"Vhot000", None, ttl);
                }
            }
            let _ = rtx.send(());
        })
    };

    // Deleter: an acked delete must stay deleted until this thread itself
    // re-inserts the key. Any Some() between ack and re-insert is a
    // resurrection.
    let (dtx, drx) = mpsc::channel();
    let deleter = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(AtomicOrdering::Acquire) {
                let _ = cache.insert(b"d000000", b"Vdel000", None, ttl);
                if cache.delete(b"d000000") {
                    for _ in 0..50 {
                        assert!(
                            cache.get(b"d000000").is_none(),
                            "acked delete resurrected during merge churn"
                        );
                        std::thread::yield_now();
                    }
                }
            }
            let _ = dtx.send(());
        })
    };

    join_within("churn writer", crx, churner, 300);
    join_within("hot-key reader", rrx, reader, 60);
    join_within("delete/verify worker", drx, deleter, 60);
}
