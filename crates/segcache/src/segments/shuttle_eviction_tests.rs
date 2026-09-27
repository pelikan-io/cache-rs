//! Shuttle models of whole eviction passes: real `Segcache` operations on
//! several threads, under randomized schedules.
//!
//! The loom and shuttle models elsewhere check the segment and hashtable
//! protocols in isolation -- a header's state machine, a slot's CAS -- with
//! stand-ins for the free queue, because crossbeam's `Injector` is invisible
//! to a model checker. Every eviction path reserves and returns segments
//! through that queue, so until `crate::sync::SegmentQueue` put it behind the
//! backend, no model could run one. These run the paths themselves: merge
//! eviction and S3-FIFO promotion, with writers forcing evictions while a
//! reader holds items hot.
//!
//! Shuttle, not loom: shuttle is sequentially consistent, so it checks the
//! invariants whose proof rests on the SeqCst Dekker pairs (a reader pin
//! against a drain claim, a writer against a drain) without the
//! store-buffering false positives loom reports for them. And a merge touches
//! too much state for loom's exhaustive search.
//!
//! After every schedule:
//!
//! - every TTL bucket chain is well-formed: links symmetric, no cycle, no
//!   segment in two chains, every chained segment readable and none left in
//!   `Relinking`;
//! - no segment leaked or was freed twice: every segment is either Free and
//!   in no chain, or in exactly one chain, and the Free count matches the
//!   queues;
//! - no item outlived its TTL: everything written in the setup phase is
//!   unreadable once its TTL has passed, however many times an eviction
//!   copied it in between.

use super::*;
use crate::clock;
use crate::eviction::Policy;
use crate::sync::shuttle_iters;
use crate::Segcache;
use core::num::NonZeroU32;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
use std::sync::Arc;
use std::time::Duration;

const KEY_LEN: usize = 7;
const VALUE: &[u8] = b"value-bytes";
const T0: u32 = 1_000_000;
const TTL: Duration = Duration::from_secs(60);

/// Clears the virtual clock when a schedule ends, pass or fail. Shuttle runs
/// its threads as continuations on the test's OS thread, so the override is
/// shared by all of them -- set once, before spawning, and read by all.
struct VirtualClock;

impl VirtualClock {
    fn at(secs: u32) -> Self {
        clock::set_virtual_now(secs);
        VirtualClock
    }

    fn set(&self, secs: u32) {
        clock::set_virtual_now(secs);
    }
}

impl Drop for VirtualClock {
    fn drop(&mut self) {
        clock::clear_virtual_now();
    }
}

fn key(prefix: char, i: usize) -> String {
    let k = format!("{prefix}{i:06}");
    assert_eq!(k.len(), KEY_LEN);
    k
}

/// A small pool, sized so the concurrent phase evicts but keeps what it
/// copies. Measured, not guessed: at 4 items a segment, a merge keeps about
/// one per candidate and the next eviction drops the spare holding it, so no
/// schedule ended with a copied item to check -- the model passed while
/// testing nothing. At 8 items, 8 segments, 48 setup items and 6 inserts per
/// writer, every schedule of both policies copied one.
struct Shape {
    items_per_segment: usize,
    segments: usize,
}

impl Shape {
    fn segment_size(&self) -> i32 {
        let item = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
        let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        (magic + item * self.items_per_segment) as i32
    }

    fn build(&self, policy: Policy) -> Segcache {
        Segcache::builder()
            .segment_size(self.segment_size())
            .heap_size(self.segment_size() as usize * self.segments)
            .hash_power(12)
            .eviction(policy)
            .eviction_seed(7)
            .build()
            .expect("failed to create cache")
    }
}

/// The segment a key currently lives in, read without bumping its frequency.
fn segment_of(cache: &Segcache, k: &str) -> Option<u32> {
    use crate::hashtable::{unpack_location, Hashtable};
    let verifier = cache.segments.verifier();
    let loc = cache
        .hashtable
        .lookup_no_freq_update(k.as_bytes(), &verifier)
        .found()?
        .location;
    Some(unpack_location(loc).0)
}

/// Schedules in which an eviction copied at least one setup item, across a
/// `check_random` run. A model whose schedules never copied anything would
/// pass its TTL check without testing it, so each test requires a floor.
struct Coverage {
    schedules: AtomicUsize,
    copied: AtomicUsize,
}

impl Coverage {
    const fn new() -> Self {
        Self {
            schedules: AtomicUsize::new(0),
            copied: AtomicUsize::new(0),
        }
    }

    fn require(&self, what: &str) {
        let (n, c) = (
            self.schedules.load(StdOrdering::Relaxed),
            self.copied.load(StdOrdering::Relaxed),
        );
        assert!(
            c * 2 >= n,
            "{what}: an eviction copied a setup item in only {c} of {n} schedules, \
             so the TTL check was mostly not exercised"
        );
    }
}

/// Walk every chain; return the set of chained segment ids.
fn assert_chains_well_formed(cache: &Segcache, total: u32) -> HashSet<u32> {
    let mut chained = HashSet::new();
    for bucket in cache.ttl_buckets.buckets.iter() {
        let mut cur = bucket.head();
        let mut prev: Option<NonZeroU32> = None;
        let mut steps = 0u32;
        while let Some(id) = cur {
            assert!(
                chained.insert(id.get()),
                "segment {id} is in two chains or a cycle"
            );
            steps += 1;
            assert!(steps <= total, "chain walk exceeded the pool: a cycle");
            let header = cache.segments.header(id);
            let state = header.state();
            assert!(state.is_readable(), "chained segment {id} is {state:?}");
            assert_ne!(state, State::Relinking, "segment {id} left in Relinking");
            assert_eq!(
                header.prev_seg(),
                prev,
                "segment {id} has an asymmetric prev link"
            );
            prev = cur;
            cur = header.next_seg();
        }
    }
    chained
}

fn assert_no_leak(cache: &Segcache, chained: &HashSet<u32>, total: u32) {
    let mut free = 0usize;
    for raw in 1..=total {
        let state = cache.segments.header(NonZeroU32::new(raw).unwrap()).state();
        if state == State::Free {
            free += 1;
            assert!(!chained.contains(&raw), "segment {raw} is Free but chained");
        } else {
            assert!(
                chained.contains(&raw),
                "segment {raw} is {state:?} but in no chain"
            );
        }
    }
    assert_eq!(
        free,
        cache.segments.free(),
        "Free segments must match the queues"
    );
    assert_eq!(
        free + chained.len(),
        total as usize,
        "free + chained must cover the pool"
    );
}

/// The shared model: setup at T0, a concurrent phase at T0 + 30 in which
/// writers force evictions while a reader keeps the setup's items hot, then
/// the structural and TTL checks.
fn run_model(
    policy: Policy,
    shape: Shape,
    old_items: usize,
    new_per_writer: usize,
    coverage: &Coverage,
) {
    let total = shape.segments as u32;
    let clock = VirtualClock::at(T0);
    let cache = Arc::new(shape.build(policy));

    let old: Vec<String> = (0..old_items).map(|i| key('o', i)).collect();
    for k in &old {
        let _ = cache.insert(k.as_bytes(), VALUE, None, TTL);
    }
    // Warm the first half, a second per read in case the counter is
    // rate-limited, so evictions have survivors to copy.
    for s in 1..=3 {
        clock.set(T0 + s);
        for k in &old[..old.len() / 2] {
            let _ = cache.get(k.as_bytes());
        }
    }

    let hot = &old[..old.len() / 2];
    let before: Vec<Option<u32>> = hot.iter().map(|k| segment_of(&cache, k)).collect();

    clock.set(T0 + 30);
    let mut handles = Vec::new();
    for w in 0..2 {
        let cache = Arc::clone(&cache);
        handles.push(shuttle::thread::spawn(move || {
            for i in 0..new_per_writer {
                let k = key(if w == 0 { 'a' } else { 'b' }, i);
                let _ = cache.insert(k.as_bytes(), VALUE, None, TTL);
            }
        }));
    }
    // Two evictors calling `evict` directly, so eviction passes overlap each
    // other and the writers' chain expansion by construction. Left to the
    // writers alone, the first eviction freed enough for both and passes
    // rarely overlapped: dropping the spare on a lost claim, linking before
    // an unclaimed s0, and merging without the chain lock all passed 1000
    // schedules undetected.
    for _ in 0..2 {
        let cache = Arc::clone(&cache);
        handles.push(shuttle::thread::spawn(move || {
            for _ in 0..2 {
                let _ = cache.segments.evict(&cache.ttl_buckets, &cache.hashtable);
            }
        }));
    }
    {
        let cache = Arc::clone(&cache);
        let hot: Vec<String> = old[..old.len() / 2].to_vec();
        handles.push(shuttle::thread::spawn(move || {
            for k in &hot {
                let _ = cache.get(k.as_bytes());
            }
        }));
    }
    for h in handles {
        h.join().expect("a model thread panicked");
    }

    // An item that is still cached but in another segment was copied by an
    // eviction -- the move whose TTL handling the final check is about.
    let copied = hot.iter().zip(&before).any(
        |(k, was)| matches!((segment_of(&cache, k), was), (Some(now), Some(was)) if now != *was),
    );
    coverage.schedules.fetch_add(1, StdOrdering::Relaxed);
    if copied {
        coverage.copied.fetch_add(1, StdOrdering::Relaxed);
    }

    let chained = assert_chains_well_formed(&cache, total);
    assert_no_leak(&cache, &chained, total);

    // Written at T0 with a 60s TTL: from T0 + 60, however an eviction moved
    // them during the concurrent phase, none may be served.
    clock.set(T0 + 60);
    for k in &old {
        assert!(
            cache.get(k.as_bytes()).is_none(),
            "{k}, written at T0 with a 60s TTL, was served at T0 + 60"
        );
    }
}

#[test]
fn shuttle_concurrent_merge_evictions() {
    static COVERAGE: Coverage = Coverage::new();
    shuttle::check_random(
        || {
            run_model(
                Policy::Merge {
                    max: 8,
                    merge: 4,
                    compact: 0,
                },
                Shape {
                    items_per_segment: 8,
                    segments: 8,
                },
                48,
                6,
                &COVERAGE,
            )
        },
        shuttle_iters(200),
    );
    COVERAGE.require("merge");
}

#[test]
fn shuttle_concurrent_s3fifo_evictions() {
    static COVERAGE: Coverage = Coverage::new();
    shuttle::check_random(
        || {
            run_model(
                Policy::S3Fifo {
                    admission_ratio: 0.25,
                },
                Shape {
                    items_per_segment: 8,
                    segments: 8,
                },
                48,
                6,
                &COVERAGE,
            )
        },
        shuttle_iters(200),
    );
    COVERAGE.require("s3fifo");
}

/// An expiry drain racing a merge on the neighbouring segments of one chain.
///
/// The models above never put two mutators on adjacent links: two merges on
/// one bucket start from the same cursor and the loser backs off at the
/// claim, and nothing expires during their concurrent phase. So the
/// per-bucket `chain_lock` went untested -- a merge with it removed passed
/// 5000 schedules. Here one chain holds two segments from T0 followed by six
/// from T0 + 30, and the merge cursor sits on the first of the six: the
/// drain of the old pair rewrites the links the merge splices its spare
/// into.
///
/// `evict` runs its own expiry pass first, under the same lock, so the two
/// only overlap if time moves mid-phase: the evictor's pass finds nothing,
/// it starts merging, and only then does the clock pass the old pair's
/// deadline for the expirer. One thread moves the clock; the override is
/// shared, since shuttle runs every thread on the test's OS thread.
///
/// Run under PCT, not uniform random scheduling. The race window is a
/// single step of the merge -- between reading s0's predecessor and linking
/// the spare to it -- and the drain must run its whole unlink of that
/// predecessor inside it. A uniform scheduler picks a thread at every step,
/// so a stretch that long never happens: with the merge's chain lock removed
/// it passed 1000 schedules in which both a drain and a merge ran. PCT
/// switches priorities at only a few points, so one thread can run far
/// while another sits mid-operation, and the same mutation fails in about a
/// second -- as an asymmetric link after the merge spliced its spare next to
/// a segment the drain had unlinked, or as the drain walking into the half-
/// linked spare.
fn run_drain_vs_merge(coverage: &Coverage, drained: &AtomicUsize) {
    let shape = Shape {
        items_per_segment: 8,
        segments: 12,
    };
    let total = shape.segments as u32;
    let clock = VirtualClock::at(T0);
    let cache = Arc::new(shape.build(Policy::Merge {
        max: 8,
        merge: 4,
        compact: 0,
    }));

    // Two segments at T0 (ids 2, 3: the spare seeded at construction is 1),
    // then six at T0 + 30 (ids 4..=9).
    let old: Vec<String> = (0..16).map(|i| key('o', i)).collect();
    for k in &old {
        cache.insert(k.as_bytes(), VALUE, None, TTL).expect("fill");
    }
    clock.set(T0 + 30);
    let young: Vec<String> = (0..48).map(|i| key('y', i)).collect();
    for k in &young {
        cache.insert(k.as_bytes(), VALUE, None, TTL).expect("fill");
    }
    for s in 31..=33 {
        clock.set(T0 + s);
        for k in &young[..24] {
            let _ = cache.get(k.as_bytes());
        }
    }
    let bucket = cache
        .ttl_buckets
        .get_bucket(clocksource::coarse::Duration::from_secs(
            TTL.as_secs() as u32
        ));
    let first_young = NonZeroU32::new(4).unwrap();
    assert_eq!(
        bucket.head(),
        NonZeroU32::new(2),
        "the old pair must head the chain"
    );
    bucket.set_next_to_merge(Some(first_young));
    let before: Vec<Option<u32>> = young[..24].iter().map(|k| segment_of(&cache, k)).collect();

    // T0 + 50: nothing has expired (the old pair's deadline is T0 + 56).
    clock.set(T0 + 50);
    let mut handles = Vec::new();
    handles.push(shuttle::thread::spawn(|| clock::set_virtual_now(T0 + 57)));
    {
        let cache = Arc::clone(&cache);
        handles.push(shuttle::thread::spawn(move || {
            for _ in 0..2 {
                cache.expire();
            }
        }));
    }
    {
        let cache = Arc::clone(&cache);
        handles.push(shuttle::thread::spawn(move || {
            for _ in 0..2 {
                let _ = cache.segments.evict(&cache.ttl_buckets, &cache.hashtable);
            }
        }));
    }
    {
        let cache = Arc::clone(&cache);
        handles.push(shuttle::thread::spawn(move || {
            for i in 0..8 {
                let _ = cache.insert(key('w', i).as_bytes(), VALUE, None, TTL);
            }
        }));
    }
    for h in handles {
        h.join().expect("a model thread panicked");
    }

    // Both halves of the race have to have happened for the schedule to
    // count: the old pair drained, and a merge copied a young item.
    let old_gone = [2u32, 3]
        .iter()
        .all(|&id| cache.segments.header(NonZeroU32::new(id).unwrap()).state() != State::Sealed);
    let merged = young[..24].iter().zip(&before).any(
        |(k, was)| matches!((segment_of(&cache, k), was), (Some(now), Some(was)) if now != *was),
    );
    coverage.schedules.fetch_add(1, StdOrdering::Relaxed);
    if merged {
        coverage.copied.fetch_add(1, StdOrdering::Relaxed);
    }
    if old_gone && merged {
        drained.fetch_add(1, StdOrdering::Relaxed);
    }

    let chained = assert_chains_well_formed(&cache, total);
    assert_no_leak(&cache, &chained, total);

    clock.set(T0 + 60);
    for k in &old {
        assert!(
            cache.get(k.as_bytes()).is_none(),
            "{k}, written at T0 with a 60s TTL, was served at T0 + 60"
        );
    }
}

#[test]
fn shuttle_expiry_drain_races_a_merge() {
    static COVERAGE: Coverage = Coverage::new();
    static BOTH: AtomicUsize = AtomicUsize::new(0);
    shuttle::check_pct(
        || run_drain_vs_merge(&COVERAGE, &BOTH),
        shuttle_iters(200),
        3,
    );
    COVERAGE.require("drain vs merge");
    let (both, n) = (
        BOTH.load(StdOrdering::Relaxed),
        COVERAGE.schedules.load(StdOrdering::Relaxed),
    );
    assert!(
        both * 2 >= n,
        "the old pair drained and a merge ran in only {both} of {n} schedules"
    );
}
