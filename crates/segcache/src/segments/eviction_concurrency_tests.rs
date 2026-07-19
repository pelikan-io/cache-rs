//! Reader-vs-eviction tests for drain-safe merge (roadmap item 5b).
//!
//! This test is single-threaded by construction. `acquire_item_at(&self) ->
//! Option<(RawItem, SegmentGuard)>` returns a guard holding raw pointers
//! (segment header + free queue), NOT a borrow of `Segments`. Once it returns,
//! the immutable borrow ends, so `evict(&mut self)` can run while the `RawItem`
//! and `SegmentGuard` stay alive — the pin-across-eviction composition item 7
//! will enable under real concurrency. No threads, no `RwLock`.
//!
//! SCOPE — what a single-threaded test can and cannot prove here. The
//! copy-to-spare rework's headline safety property ("a merge never moves a
//! readable segment's live bytes under a *racing* reader pin") is inherently
//! CONCURRENT and is NOT tested here. The reason: the merge's gates
//! (`merge_evict_chain_len`, then `can_evict`) observe any *pre-existing* pin
//! and bail BEFORE processing that segment — so single-threaded, neither the
//! old in-place `compact()` nor the new copy-to-spare merge ever mutates a
//! segment a reader has already pinned. The old-vs-new difference only appears
//! when a pin RACES IN after the gate check, a window that requires the `&self`
//! concurrency of item 7. There is no sound single-threaded test that
//! distinguishes copy-to-spare from in-place compaction (reading a
//! recycled/unpinned pointer post-drain would be racy/unsound). That
//! verification is deferred to item 7, where a concurrent/loom test can
//! exercise the racing-pin window.

use super::*;
use crate::eviction::Policy;
use crate::Segcache;
use core::num::NonZeroU32;
use keyvalue::Value;
use std::time::Duration;

/// Pin an item X in a LATER merge candidate and run a full `evict()` pass while
/// the pin is held. This proves the following (all genuinely single-threaded
/// properties — see the module SCOPE note for what is deliberately NOT proven):
///
/// - the merge does real copy-to-spare work on the *unpinned* candidates
///   (seg2/3/4 → Free; their survivors are relocated into the spare and remain
///   reachable via `get()` with their correct distinct values);
/// - the merge HALTS CLEANLY at the pinned candidate: a pin makes `can_evict()`
///   false, so the candidate loop stops at seg5 and leaves it Sealed, pinned,
///   and byte-for-byte intact (it is never drained or condemned);
/// - a reader pin held across a full `evict()` keeps its segment alive and
///   readable — the guard / raw-pointer mechanism composes with eviction;
/// - no leak (free + spare + readable == total) and the pool stays consistent
///   after the guard drops.
#[test]
fn merge_halts_at_pinned_candidate_and_relocates_survivors() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
    const VAL_LEN: usize = 7; // "V" + 6 zero-padded digits

    // Distinct value per key so a torn/moved/relocated read is detectable: a
    // reader that saw a neighbouring item's bytes, or stale/shifted bytes,
    // would mismatch its key's known value.
    let key_of = |i: usize| format!("k{i:06}");
    let val_of = |i: usize| format!("V{i:06}");
    let sample_val = val_of(0);
    assert_eq!(sample_val.len(), VAL_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Layout: segment id 1 is the held-back spare (Merge policy); reserve_free
    // hands out ids 2.. for the fill. Five filled segments:
    //   seg2, seg3, seg4  -> drained candidates (copy-to-spare)
    //   seg5              -> holds the pinned reader item X (Sealed)
    //   seg6              -> the Live write tail (never a candidate)
    let free_segments = 5usize;
    let total_segments = free_segments + 1; // + 1 held-back spare

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.segments.free_only(), free_segments);
    assert_eq!(cache.segments.spare_count(), 1);

    // Long TTL: nothing expires, so evict() falls through the expire-first
    // fast path into the actual merge.
    let ttl = Duration::from_secs(3600);

    // Fill every normal free segment exactly full; all share one TTL bucket.
    // key i lands in segment (2 + i / ITEMS_PER_SEGMENT), the last segment
    // staying the Live write tail.
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    for i in 0..fill_count {
        let key = key_of(i);
        let val = val_of(i);
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, ttl)
            .expect("fill inserts must succeed without needing eviction");
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must exactly exhaust the free queue"
    );

    // Leave exactly ONE survivor per drained candidate (the first key of
    // seg2/seg3/seg4) and delete the rest. This keeps each candidate's live
    // bytes tiny, so copying all three survivors into the spare stays well
    // below the merge's stop-bytes high-watermark — the candidate loop then
    // reaches and drains seg2/seg3/seg4 and only STOPS at the pinned seg5
    // (can_evict == false), rather than filling the spare after one candidate.
    // A single live item is also below the prune keep-ratio, so it is never
    // pruned; it is copied into the spare and republished via the hashtable.
    let survivor_idx = [0usize, ITEMS_PER_SEGMENT, 2 * ITEMS_PER_SEGMENT];
    for i in 0..3 * ITEMS_PER_SEGMENT {
        if !survivor_idx.contains(&i) {
            assert!(
                cache.delete(key_of(i).as_bytes()),
                "delete of a fill key must succeed"
            );
        }
    }
    for &i in &survivor_idx {
        assert!(
            cache.get(key_of(i).as_bytes()).is_some(),
            "survivor key must still resolve before the merge"
        );
    }

    // ── Pin item X in a LATER candidate (seg5), NOT the first candidate and
    //    NOT the Live tail. X is the first item of seg5, at offset
    //    magic_overhead. The three unpinned candidates ahead of it (seg2/3/4)
    //    give merge_evict_chain_len >= 3 so the merge proceeds; the pin on
    //    seg5 then STOPS the candidate loop at seg5 (can_evict == false).
    let x_seg = NonZeroU32::new(5).unwrap();
    let x_idx = 3 * ITEMS_PER_SEGMENT; // first key of seg5
    let x_key = key_of(x_idx);
    let x_val = val_of(x_idx);

    assert_eq!(
        cache.segments.header(x_seg).state(),
        State::Sealed,
        "precondition: X's segment must be Sealed (readable, not the Live tail)"
    );

    let (raw_item_x, guard_x) = cache
        .segments
        .acquire_item_at(x_seg, magic_overhead)
        .expect("X's segment must be readable and pinnable");

    // The pin is live: ref_count bumped.
    assert!(
        cache.segments.header(x_seg).ref_count() >= 1,
        "acquire_item_at must pin X's segment (ref_count >= 1)"
    );

    // The bytes at X match its known key/value NOW (validates the layout
    // assumption; a wrong offset fails loudly here rather than passing).
    assert_eq!(
        raw_item_x.key(),
        x_key.as_bytes(),
        "X key mismatch pre-merge"
    );
    assert_value_eq(raw_item_x.value(), x_val.as_bytes(), "X value pre-merge");
    #[cfg(feature = "integrity")]
    raw_item_x.check_magic();

    // Record which candidates are Sealed before the merge (seg2/3/4).
    let drained_candidates = [
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(3).unwrap(),
        NonZeroU32::new(4).unwrap(),
    ];
    for &c in &drained_candidates {
        assert_eq!(
            cache.segments.header(c).state(),
            State::Sealed,
            "candidate {c} must be Sealed before the merge"
        );
    }

    // ── Run one merge pass. It drains seg2/3/4 via copy-to-spare and STOPS at
    //    the pinned seg5. raw_item_x / guard_x hold raw pointers, not a borrow
    //    of cache.segments, so this &mut call compiles while the pin is live.
    cache
        .segments
        .evict(&mut cache.ttl_buckets, &cache.hashtable)
        .expect("merge eviction must succeed on the 3-candidate prefix");

    // ── With guard_x STILL ALIVE, re-read X through the pinned pointer. The
    //    merge halted at seg5 (can_evict == false) without draining it, so the
    //    bytes MUST be byte-for-byte identical. NOTE: this does not by itself
    //    distinguish copy-to-spare from in-place compaction — the merge gates
    //    on the pre-existing pin and never processes seg5 under EITHER design
    //    single-threaded (see the module SCOPE note). It verifies the clean
    //    halt: the pinned candidate is left untouched.
    assert_eq!(
        raw_item_x.key(),
        x_key.as_bytes(),
        "pinned candidate's KEY must be untouched by the halted merge"
    );
    assert_value_eq(
        raw_item_x.value(),
        x_val.as_bytes(),
        "pinned candidate's VALUE must be untouched by the halted merge",
    );
    #[cfg(feature = "integrity")]
    raw_item_x.check_magic();

    // The pinned candidate was NOT condemned (it was never drained) — it stays
    // Sealed and pinned throughout.
    assert_eq!(
        cache.segments.header(x_seg).state(),
        State::Sealed,
        "the pinned candidate must stay Sealed (merge stops at it, never drains it)"
    );
    assert!(
        cache.segments.header(x_seg).ref_count() >= 1,
        "the pin must still be held after the merge"
    );

    // ── Copy-to-spare actually ran: the earlier candidates were drained.
    let freed = (1..=total_segments as u32)
        .filter(|&id| cache.segments.header(NonZeroU32::new(id).unwrap()).state() == State::Free)
        .count();
    assert!(
        freed >= 1,
        "merge must have drained at least one candidate via copy-to-spare"
    );
    for &c in &drained_candidates {
        assert_eq!(
            cache.segments.header(c).state(),
            State::Free,
            "candidate {c} must have been drained (copy-to-spare) to Free"
        );
    }

    // ── (5) The drained candidates' survivors were published into the spare
    //    (copy-then-relink) and are reachable with their correct distinct
    //    values. get() takes &mut self; guard_x/raw_item_x hold raw pointers,
    //    not a borrow of cache, so this is fine while the pin is live.
    for &i in &survivor_idx {
        let item = cache
            .get(key_of(i).as_bytes())
            .unwrap_or_else(|| panic!("survivor key {} must survive the merge", key_of(i)));
        assert_value_eq(
            item.value(),
            val_of(i).as_bytes(),
            "relocated survivor must keep its distinct value",
        );
    }

    // ── (6) No leak: every segment is accounted for as available or readable.
    let free_after = cache.segments.free();
    let readable = (1..=total_segments as u32)
        .filter(|&id| {
            cache
                .segments
                .header(NonZeroU32::new(id).unwrap())
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        free_after + readable,
        total_segments,
        "no leak: available + readable segments must account for the whole pool"
    );

    // ── (7) Release the pin; the cache stays consistent. The pin is held by
    //    the SegmentGuard — `RawItem` is `Copy`, so dropping it does nothing;
    //    dropping the guard is what decrements ref_count. X's segment was never
    //    condemned, so it simply becomes unpinned (ref_count -> 0) and remains
    //    readable; a follow-up lookup still works.
    let _ = raw_item_x;
    drop(guard_x);
    assert_eq!(
        cache.segments.header(x_seg).ref_count(),
        0,
        "dropping the guard must release the pin"
    );
    // A survivor lookup still succeeds after the pin is released.
    let i = survivor_idx[0];
    let item = cache
        .get(key_of(i).as_bytes())
        .expect("cache must stay consistent after the pin is released");
    assert_value_eq(
        item.value(),
        val_of(i).as_bytes(),
        "post-drop survivor value",
    );

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("cache must pass integrity check");
}

/// Assert a byte `Value` equals `expected`.
fn assert_value_eq(v: Value, expected: &[u8], msg: &str) {
    match v {
        Value::Bytes(b) => assert_eq!(b, expected, "{msg}"),
        other => panic!("{msg}: expected bytes {expected:?}, got {other:?}"),
    }
}
