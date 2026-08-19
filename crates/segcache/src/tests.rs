// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

use super::*;
use crate::hashtable::bucket::Hashbucket;
use ::rand::Rng;
use core::num::NonZeroU32;
use keyvalue::ITEM_HDR_SIZE;

use std::time::Duration;

#[test]
fn sizes() {
    // ITEM_HDR_SIZE is 12 with integrity (keyvalue default) or 6 without.
    assert!(matches!(ITEM_HDR_SIZE, 6 | 12));

    assert_eq!(std::mem::size_of::<SegmentHeader>(), 64);

    assert_eq!(std::mem::size_of::<Hashbucket>(), 64);

    assert_eq!(std::mem::size_of::<crate::ttl_buckets::TtlBucket>(), 64);
    assert_eq!(std::mem::size_of::<TtlBuckets>(), 24);
}

// The generation is spent by the transitions that end a *used*
// incarnation, so CAS tokens from a previous use of the segment can never
// match again. Reserving does not spend one, and neither does handing back
// a segment that was never written into.
#[test]
fn segment_header_generation_bumps_when_used_incarnation_ends() {
    use crate::segments::state::State;

    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());
    assert_eq!(header.generation(), 0);

    // Free -> Reserved: no bump.
    assert!(header.try_reserve());
    assert_eq!(header.generation(), 0);

    // Reserved -> Free (the chain-extension election loser): still no
    // bump — nothing was ever published into this incarnation.
    assert!(header.try_release());
    assert_eq!(header.generation(), 0);

    // A full use: reserve, run it up to Draining, then Draining -> Free.
    assert!(header.try_reserve());
    header.set_state(State::Draining);
    assert_eq!(header.generation(), 0);
    assert!(header.try_release_drained());
    assert_eq!(header.state(), State::Free);
    assert_eq!(header.generation(), 1);

    // The reader-pinned variant of the same event: AwaitingRelease -> Free
    // bumps too, which is what covers the guard-drop free that never
    // passes through `Segments::recycle`.
    assert!(header.try_reserve());
    header.set_state(State::AwaitingRelease);
    assert_eq!(header.generation(), 1);
    assert!(header.try_release_condemned());
    assert_eq!(header.state(), State::Free);
    assert_eq!(header.generation(), 2);

    // Losing the release CAS must not bump: only the winner does, which is
    // what keeps the condemned handoff exactly-one-free *and* exactly-one-bump.
    assert!(!header.try_release_condemned());
    assert!(!header.try_release_drained());
    assert_eq!(header.generation(), 2);
}

// A held Item pins its segment: heavy eviction churn must neither move
// nor recycle the pinned segment, so the held value stays readable and
// the key's CAS token (location + generation) is unchanged.
#[test]
fn pinned_segment_survives_eviction_churn() {
    let segment_size = 4096;
    let segments = 8;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("failed to create cache");

    // canary lands in the oldest segment — Fifo's first victim
    assert!(cache.insert(b"pinned", b"canary", None, ttl).is_ok());
    let item = cache.get(b"pinned").unwrap();
    let token = item.cas();

    // churn roughly 10x the heap through the cache; every insert must
    // succeed because all other segments remain evictable
    let filler = [0xABu8; 128];
    for i in 0..2000u32 {
        let key = format!("filler_{i}");
        cache
            .insert(key.as_bytes(), &filler[..], None, ttl)
            .expect("insert must succeed while only one segment is pinned");
    }

    // the held item's bytes never moved
    assert_eq!(item.value(), b"canary");

    // the key still resolves, at the same location and generation
    let fresh = cache.get(b"pinned").unwrap();
    assert_eq!(fresh.value(), b"canary");
    assert_eq!(
        fresh.cas(),
        token,
        "pinned segment was moved or recycled during churn"
    );
}

// clear() must always drain the hashtable, but a pinned segment is not
// freed until its readers drop; the held Item keeps reading its bytes,
// and the guard drop itself frees the condemned segment.
#[test]
fn pinned_segment_survives_clear() {
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"coffee", b"strong", None, ttl).is_ok());
    let item = cache.get(b"coffee").unwrap();

    // pinned segment is drained but not freed
    assert_eq!(cache.clear(), 0);
    assert_eq!(cache.segments.free(), segments - 1);
    assert_eq!(item.value(), b"strong");
    assert!(cache.get(b"coffee").is_none());

    // the condemned tail was unlinked; inserting must expand into a
    // fresh segment rather than spin
    assert!(cache.insert(b"tea", b"green", None, ttl).is_ok());
    assert!(cache.get(b"tea").is_some());
    assert_eq!(cache.segments.free(), segments - 2);

    // the guard drop completes the AwaitingRelease handoff: the
    // condemned segment returns to the free queue immediately, with no
    // further expire/clear/eviction pass
    drop(item);
    assert_eq!(cache.segments.free(), segments - 1);

    cache.clear();
    assert_eq!(cache.segments.free(), segments);
}

// Numeric updates are in place: a held Item aliases the same memory and
// observes increments through the seqlock — reads are never torn, and
// the pinned segment cannot be reclaimed while held.
#[test]
fn held_item_observes_inplace_increments() {
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"n", 0, Some(b"opt"), ttl).is_ok());

    let held = cache.get(b"n").unwrap();
    assert_eq!(held.value(), 0);

    // in-place updates are visible through the held alias
    assert_eq!(cache.wrapping_add(b"n", 1).unwrap(), 1);
    assert_eq!(held.value(), 1);
    assert_eq!(cache.wrapping_add(b"n", 1).unwrap(), 2);
    assert_eq!(held.value(), 2);

    // optional data is untouched by increments
    assert_eq!(held.optional(), Some(&b"opt"[..]));

    // the pin still protects the segment
    cache.clear();
    assert_eq!(cache.segments.free(), segments - 1);
    assert_eq!(held.value(), 2);

    drop(held);
    assert_eq!(cache.segments.free(), segments);
}

// The seal happens on append: while a segment is the bucket tail it is
// Live (writable, never evictable); the moment a successor is linked it
// becomes Sealed (readable, evictable). This replaces the old
// "has a next segment" eviction guard.
#[test]
fn seal_on_append() {
    let segment_size = 4096;
    let segments = 8;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    // fill past one segment so the bucket has at least two
    let filler = [0xCDu8; 256];
    for i in 0..30u32 {
        let key = format!("key_{i}");
        assert!(cache.insert(key.as_bytes(), &filler[..], None, ttl).is_ok());
    }
    assert!(
        cache.segments.free() <= segments - 2,
        "need two used segments"
    );

    // FIFO reservation hands out ids 1, 2, 3, ... in order, so the
    // highest used id is the current tail and every earlier segment was
    // sealed when its successor was appended.
    let used = segments - cache.segments.free();
    for id in 1..used as u32 {
        let seg = cache
            .segments
            .segment(NonZeroU32::new(id).unwrap())
            .unwrap();
        assert_eq!(
            seg.state(),
            State::Sealed,
            "predecessor {id} must be sealed"
        );
        assert!(seg.can_evict());
    }

    let tail = cache
        .segments
        .segment(NonZeroU32::new(used as u32).unwrap())
        .unwrap();
    assert_eq!(tail.state(), State::Live);
    assert!(!tail.can_evict(), "the write tail must never be evictable");
}

// cas() publishes by swapping the hashtable slot from the token-checked
// location — so if the eviction triggered by cas's OWN reservation
// relocates or evicts the checked item, the CAS fails with Exists
// (fail-safe) instead of silently succeeding through a plain insert.
#[test]
fn cas_fails_when_own_reservation_evicts_checked_item() {
    let segment_size = 4096;
    let segments = 2;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("failed to create cache");

    // target lands in segment 1 — the oldest, Fifo's first victim
    assert!(cache.insert(b"target", b"original", None, ttl).is_ok());
    let token = cache.get(b"target").unwrap().cas();

    // fill the heap so the next reservation must evict: keep inserting
    // fillers until the Live tail can't fit another large item
    let filler = [0xEEu8; 128];
    // stop filling once the tail can no longer fit the cas item below
    // (header 12 + key 6 + value 600, rounded up -> 624 bytes)
    let needed = 624;
    let mut i = 0u32;
    loop {
        let tail_free = {
            let used = segments - cache.segments.free();
            let tail = cache
                .segments
                .segment(NonZeroU32::new(used as u32).unwrap())
                .unwrap();
            segment_size as usize - tail.write_offset() as usize
        };
        if cache.segments.free() == 0 && tail_free < needed {
            break;
        }
        let key = format!("filler_{i}");
        cache
            .insert(key.as_bytes(), &filler[..], None, ttl)
            .expect("setup insert must succeed");
        i += 1;
    }

    // the token is still valid right now
    assert_eq!(cache.get(b"target").unwrap().cas(), token);

    // cas must reserve, reservation must evict, Fifo evicts segment 1
    // (the sealed oldest — where target lives) — the checked location no
    // longer maps to the key, so the CAS fails closed
    let big = [0xFFu8; 600];
    assert_eq!(
        cache.cas(b"target", &big[..], None, ttl, token),
        Err(SegcacheError::Exists)
    );

    // the target was evicted, not replaced
    assert!(cache.get(b"target").is_none());

    // and the cache remains fully usable
    assert!(cache.insert(b"after", b"ok", None, ttl).is_ok());
    assert_eq!(cache.get(b"after").unwrap().value(), b"ok");
}

// Every increment writes a new item and bumps the key's CAS token —
// matching memcached, where incr/decr assign a fresh cas unique. A
// gets -> (incr) -> cas sequence must fail, not silently discard the
// increment.
#[test]
fn incr_bumps_cas_token() {
    let ttl = Duration::ZERO;
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"counter", 5, None, ttl).is_ok());
    let stale = cache.get(b"counter").unwrap().cas();

    assert_eq!(cache.wrapping_add(b"counter", 1).unwrap(), 6);
    let fresh_token = cache.get(b"counter").unwrap().cas();
    assert_ne!(fresh_token, stale, "increment must bump the CAS token");

    // the pre-increment token must not match anymore
    assert_eq!(
        cache.cas(b"counter", 100, None, ttl, stale),
        Err(SegcacheError::Exists)
    );
    assert_eq!(cache.get(b"counter").unwrap().value(), 6);

    // a fresh token works
    let fresh = cache.get(b"counter").unwrap().cas();
    assert_eq!(cache.cas(b"counter", 100, None, ttl, fresh), Ok(()));
    assert_eq!(cache.get(b"counter").unwrap().value(), 100);
}

// Numeric updates preserve the item's ABSOLUTE expiration exactly
// (memcached's incr/decr keep the original exptime): the update is in
// place, so the item's location — and therefore its segment deadline —
// never changes. A rate-limiter window still resets on schedule.
#[test]
fn numeric_update_preserves_expiry() {
    let ttl = Duration::from_secs(300);
    let mut cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"counter", 7, None, ttl).is_ok());

    let location_of = |cache: &mut Segcache, key: &[u8]| {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(key, &verifier)
            .unwrap();
        loc
    };

    let before = location_of(&mut cache, b"counter");
    assert_eq!(cache.wrapping_add(b"counter", 1).unwrap(), 8);
    let after = location_of(&mut cache, b"counter");

    // in place: same location, same segment, same deadline
    assert_eq!(
        after.as_raw(),
        before.as_raw(),
        "in-place increment must not move the item"
    );
}

// Incrementing a counter whose deadline has already passed returns
// NotFound, matching memcached's treatment of expired keys — even if
// the segment has not been reclaimed by expire() yet.
#[test]
fn numeric_update_expired_counter_not_found() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    // 2s requests the first tier-1 bucket, whose floor TTL is 1s
    assert!(cache
        .insert(b"counter", 5, None, Duration::from_secs(2))
        .is_ok());

    std::thread::sleep(std::time::Duration::from_secs(3));

    assert!(matches!(
        cache.wrapping_add(b"counter", 1),
        Err(SegcacheError::NotFound)
    ));
}

// Lazy expiry on the read path: an item past its segment deadline acts
// missing on get/get_no_freq_incr, matching memcached — even if nothing
// (expire(), eviction pressure) has reclaimed the segment yet.
#[test]
fn lazy_expiry_get_expired_item_not_found() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    // 2s requests the first tier-1 bucket, whose floor TTL is 1s
    assert!(cache
        .insert(b"latte", b"hot", None, Duration::from_secs(2))
        .is_ok());
    assert!(cache.get(b"latte").is_some());

    std::thread::sleep(std::time::Duration::from_secs(3));

    // no expire() call — the deadline alone makes the item invisible
    assert!(cache.get(b"latte").is_none());
    assert!(cache.get_no_freq_incr(b"latte").is_none());
}

// Lazy expiry on cas: a cas against an expired item fails NotFound
// (memcached returns NOT_FOUND for cas on an expired key), even with
// the token that was valid before the deadline passed.
#[test]
fn lazy_expiry_cas_expired_item_not_found() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache
        .insert(b"latte", b"hot", None, Duration::from_secs(2))
        .is_ok());
    let token = cache.get(b"latte").expect("not found").cas();

    std::thread::sleep(std::time::Duration::from_secs(3));

    assert_eq!(
        cache.cas(b"latte", b"cold", None, Duration::from_secs(2), token),
        Err(SegcacheError::NotFound)
    );
}

// Lazy expiry on delete: deleting an expired item reports false
// (memcached: DELETE on an expired key -> NOT_FOUND).
#[test]
fn lazy_expiry_delete_expired_item_not_found() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache
        .insert(b"latte", b"hot", None, Duration::from_secs(2))
        .is_ok());

    std::thread::sleep(std::time::Duration::from_secs(3));

    assert!(!cache.delete(b"latte"));
}

// Control: `Duration::ZERO` means "never expires" — get/cas/delete all
// behave normally after the same sleep the expiry tests use.
#[test]
fn lazy_expiry_zero_ttl_never_expires() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"latte", b"hot", None, Duration::ZERO).is_ok());

    std::thread::sleep(std::time::Duration::from_secs(3));

    let item = cache.get(b"latte").expect("not found");
    assert_eq!(item.value(), b"hot");
    let token = item.cas();
    drop(item);
    assert!(cache.get_no_freq_incr(b"latte").is_some());
    assert!(cache
        .cas(b"latte", b"cold", None, Duration::ZERO, token)
        .is_ok());
    assert!(cache.delete(b"latte"));
}

#[test]
fn try_into_numeric_arms() {
    let ttl = Duration::ZERO;
    let other_ttl = Duration::from_secs(60);
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    // arm 1: missing key -> created with initial and the caller's ttl
    assert_eq!(cache.try_into_numeric(b"fresh", 42, ttl), Ok(()));
    assert_eq!(cache.get(b"fresh").unwrap().value(), 42);
    // and it is incrementable
    assert_eq!(cache.wrapping_add(b"fresh", 1).unwrap(), 43);

    // arm 2: existing numeric -> no-op (value and token untouched)
    let before = cache.get(b"fresh").unwrap().cas();
    assert_eq!(cache.try_into_numeric(b"fresh", 999, ttl), Ok(()));
    assert_eq!(cache.get(b"fresh").unwrap().value(), 43);
    assert_eq!(cache.get(b"fresh").unwrap().cas(), before);

    // arm 3: simple-ASCII bytes -> converted with the SAME value, in the
    // existing item's TTL bucket (caller ttl deliberately ignored),
    // optional preserved
    assert!(cache.insert(b"ascii", b"123", Some(b"opt"), ttl).is_ok());
    let old_bucket_ttl = {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(b"ascii", &verifier)
            .unwrap();
        let (seg, _) = unpack_location(loc);
        cache
            .segments
            .segment(NonZeroU32::new(seg).unwrap())
            .unwrap()
            .ttl()
    };
    assert_eq!(cache.try_into_numeric(b"ascii", 0, other_ttl), Ok(()));
    let item = cache.get(b"ascii").unwrap();
    assert_eq!(item.value(), 123);
    assert_eq!(item.optional(), Some(&b"opt"[..]));
    drop(item);
    let new_bucket_ttl = {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(b"ascii", &verifier)
            .unwrap();
        let (seg, _) = unpack_location(loc);
        cache
            .segments
            .segment(NonZeroU32::new(seg).unwrap())
            .unwrap()
            .ttl()
    };
    assert_eq!(new_bucket_ttl, old_bucket_ttl);
    // converted key is incrementable
    assert_eq!(cache.wrapping_add(b"ascii", 2).unwrap(), 125);

    // arm 4: non-numeric bytes -> NotNumeric, item untouched
    assert!(cache.insert(b"text", b"not a number", None, ttl).is_ok());
    assert_eq!(
        cache.try_into_numeric(b"text", 0, ttl),
        Err(SegcacheError::NotNumeric)
    );
    assert_eq!(cache.get(b"text").unwrap().value(), b"not a number");

    // non-canonical numerics are rejected too (leading zero)
    assert!(cache.insert(b"zeroes", b"007", None, ttl).is_ok());
    assert_eq!(
        cache.try_into_numeric(b"zeroes", 0, ttl),
        Err(SegcacheError::NotNumeric)
    );
}

#[test]
fn can_evict_respects_ref_count() {
    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());
    header.set_state(State::Sealed);
    assert!(header.can_evict());

    // only Sealed is evictable — the Live tail never is
    header.set_state(State::Live);
    assert!(!header.can_evict());
    header.set_state(State::Sealed);

    assert_eq!(
        header.try_acquire_reader(),
        crate::segments::AcquireOutcome::Acquired
    );
    assert!(!header.can_evict());

    header.release_reader();
    assert!(header.can_evict());
}

#[test]
fn reader_pin_acquire_release() {
    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());

    // acquisition succeeds in readable states and counts pins
    header.set_state(State::Live);
    assert_eq!(
        header.try_acquire_reader(),
        crate::segments::AcquireOutcome::Acquired
    );
    assert_eq!(
        header.try_acquire_reader(),
        crate::segments::AcquireOutcome::Acquired
    );
    assert_eq!(header.ref_count(), 2);

    header.release_reader();
    header.release_reader();
    assert_eq!(header.ref_count(), 0);

    // acquisition fails in non-readable states and leaves no pin
    header.set_state(State::Draining);
    assert_ne!(
        header.try_acquire_reader(),
        crate::segments::AcquireOutcome::Acquired
    );
    assert_eq!(header.ref_count(), 0);

    header.set_state(State::Free);
    assert_ne!(
        header.try_acquire_reader(),
        crate::segments::AcquireOutcome::Acquired
    );
    assert_eq!(header.ref_count(), 0);
}

#[test]
fn init() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
}

#[test]
fn get_free_seg() {
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    let seg = cache.segments.reserve_free();
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(seg, NonZeroU32::new(1));
}

#[test]
fn try_alloc_item_bounds_and_grants() {
    use crate::segments::AllocOutcome;

    let segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 4)
        .build()
        .expect("build segments");

    let id = segments.reserve_free().expect("free segment");
    // `try_alloc_item` now pins a writer, which requires the Live state
    // (mirroring the `reserve()` caller, which only calls it once the tail
    // is writable) — a freshly reserved segment starts in `Reserved`.
    segments.header(id).set_state(State::Live);

    // live_bytes starts at the initial offset (0, or 8 with `integrity`)
    let live_bytes_before = segments.header(id).live_bytes();

    // grants are sequential and within capacity
    let a = match segments.try_alloc_item(id, 64) {
        AllocOutcome::Reserved(r) => r,
        other => panic!("expected Reserved, got {other:?}"),
    };
    let b = match segments.try_alloc_item(id, 64) {
        AllocOutcome::Reserved(r) => r,
        other => panic!("expected Reserved, got {other:?}"),
    };
    assert_eq!(b.offset(), a.offset() + 64);
    assert_eq!(a.seg(), id);

    // an oversized request fails and does not move the offset
    let before = segments.header(id).write_offset();
    assert!(matches!(
        segments.try_alloc_item(id, 4096),
        AllocOutcome::Full
    ));
    assert_eq!(segments.header(id).write_offset(), before);

    // live statistics track successful grants only
    assert_eq!(segments.header(id).live_items(), 2);
    assert_eq!(segments.header(id).live_bytes(), live_bytes_before + 128);
}

// try_alloc_item now pins a writer (Dekker pair, item 7d): the pin is held
// only across the reserve→publish window and must be released whether the
// call grants space (Reserved, dropped by the caller) or finds the segment
// full (Full, dropped internally before returning) — never leaked.
#[test]
fn try_alloc_item_pins_writer_until_dropped() {
    use crate::segments::AllocOutcome;

    let segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 4)
        .build()
        .expect("build segments");

    let seg = segments.reserve_free().expect("free segment");
    segments.header(seg).set_state(State::Live);

    assert_eq!(segments.header(seg).active_writers(), 0);

    match segments.try_alloc_item(seg, 64) {
        AllocOutcome::Reserved(r) => {
            assert_eq!(
                segments.header(seg).active_writers(),
                1,
                "pinned while reserved"
            );
            drop(r);
            assert_eq!(segments.header(seg).active_writers(), 0, "released on drop");
        }
        other => panic!("expected Reserved, got {other:?}"),
    }

    // Fill the segment so the next alloc returns Full, and assert the pin
    // is released (not leaked) on the Full path.
    let seg_size = segments.segment_size();
    loop {
        match segments.try_alloc_item(seg, seg_size / 4) {
            AllocOutcome::Reserved(r) => drop(r),
            AllocOutcome::Full => break,
            AllocOutcome::NotWritable => panic!("unexpected NotWritable while Live"),
        }
    }
    assert_eq!(
        segments.header(seg).active_writers(),
        0,
        "Full path must not leak a pin"
    );
}

#[test]
fn get() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.get(b"coffee").is_none());
    assert!(cache.insert(b"coffee", b"strong", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    assert!(cache.get(b"coffee").is_some());

    let item = cache.get(b"coffee").unwrap();
    assert_eq!(item.value(), b"strong", "item is: {item:?}");
}

#[test]
fn cas() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.get(b"coffee").is_none());
    assert_eq!(
        cache.cas(b"coffee", b"hot", None, ttl, 0),
        Err(SegcacheError::NotFound)
    );
    assert!(cache.insert(b"coffee", b"hot", None, ttl).is_ok());
    assert_eq!(
        cache.cas(b"coffee", b"iced", None, ttl, 0),
        Err(SegcacheError::Exists)
    );
    let item = cache.get(b"coffee").unwrap();
    assert_eq!(cache.cas(b"coffee", b"iced", None, ttl, item.cas()), Ok(()));
}

// A stale CAS token must not match after its segment is recycled, even if
// the same key lands at the same location (segment id + offset). Without
// the per-segment generation counter in the token, the identical location
// bits would make the stale token falsely succeed (ABA).
#[test]
fn cas_stale_token_rejected_after_segment_recycle() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    // A single-segment heap forces recycling to reuse the same segment
    // (the free queue is FIFO, so with more segments the next insert
    // would land elsewhere and not reproduce the ABA scenario).
    let segments = 1;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"coffee", b"hot", None, ttl).is_ok());
    let stale = cache.get(b"coffee").unwrap().cas();

    // clear() drains and frees the only segment, and the reservation on
    // the next insert bumps its generation. The same key is then written
    // at the same offset in the same segment, reproducing the identical
    // 44-bit location.
    cache.clear();
    assert_eq!(cache.segments.free(), segments);
    assert!(cache.get(b"coffee").is_none());

    assert!(cache.insert(b"coffee", b"cold", None, ttl).is_ok());
    let fresh = cache.get(b"coffee").unwrap().cas();

    // Precondition: this really is the ABA scenario — same location bits.
    // If free-queue ordering ever changes, fail loudly here rather than
    // silently passing without exercising ABA.
    assert_eq!(
        stale & CasToken::LOCATION_MASK,
        fresh & CasToken::LOCATION_MASK,
        "test precondition violated: item did not land at the same location"
    );
    assert_ne!(stale, fresh, "generation must differentiate the tokens");

    // The actual regression: with location-only tokens this falsely
    // returned Ok(()) and replaced a value the client never observed.
    assert_eq!(
        cache.cas(b"coffee", b"iced", None, ttl, stale),
        Err(SegcacheError::Exists)
    );
    assert_eq!(cache.get(b"coffee").unwrap().value(), b"cold");

    // The fresh token still works.
    assert_eq!(cache.cas(b"coffee", b"iced", None, ttl, fresh), Ok(()));
    assert_eq!(cache.get(b"coffee").unwrap().value(), b"iced");
}

#[test]
fn overwrite() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.get(b"drink").is_none());

    println!("==== first insert ====");
    assert!(cache.insert(b"drink", b"coffee", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink");
    assert!(item.is_some());
    let item = item.unwrap();
    let value = item.value();
    assert_eq!(value, b"coffee", "item is: {item:?}");

    println!("==== second insert ====");
    assert!(cache.insert(b"drink", b"espresso", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink");
    assert!(item.is_some());
    let item = item.unwrap();
    let value = item.value();
    assert_eq!(value, b"espresso", "item is: {item:?}");

    println!("==== third insert ====");
    assert!(cache.insert(b"drink", b"whisky", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink");
    assert!(item.is_some());
    let item = item.unwrap();
    let value = item.value();
    assert_eq!(value, b"whisky", "item is: {item:?}");
}

#[test]
fn delete() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.get(b"drink").is_none());

    assert!(cache.insert(b"drink", b"coffee", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink");
    assert!(item.is_some());
    let item = item.unwrap();
    let value = item.value();
    assert_eq!(value, b"coffee", "item is: {item:?}");

    assert!(cache.delete(b"drink"));
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 0);
}

#[test]
fn collisions_2() {
    let ttl = Duration::ZERO;
    let segment_size = 64;
    let segments = 2;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .hash_power(7)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 2);

    // With very small segments (64 bytes) and only 2 segments, we can
    // only hold a few items. Repeatedly overwrite 3 keys to exercise
    // the insert-replace path.
    for i in 0..1000 {
        let i = i % 3;
        let v = format!("{i:02}");
        assert!(cache.insert(v.as_bytes(), v.as_bytes(), None, ttl).is_ok());
        let item = cache.get(v.as_bytes());
        assert!(item.is_some());
    }
}

#[test]
fn collisions() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    // With the N-choice hashtable, hash_power(7) gives 2^7 = 128 slots
    // across 16 buckets with 2-choice hashing. Insert items until the
    // hashtable is full.
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .hash_power(7)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);

    // Insert items until the hashtable is full
    let mut inserted = 0;
    for i in 0..256 {
        let v = format!("{i}");
        if cache.insert(v.as_bytes(), v.as_bytes(), None, ttl).is_ok() {
            let item = cache.get(v.as_bytes());
            assert!(item.is_some());
            inserted += 1;
        } else {
            break;
        }
    }
    assert!(inserted > 0, "should have inserted at least one item");
    assert_eq!(cache.items(), inserted);

    // Deleting an item should free a slot
    let v0 = b"0";
    assert!(cache.delete(v0));
    assert_eq!(cache.items(), inserted - 1);
}

#[test]
fn full_cache_long() {
    let ttl = Duration::ZERO;
    let iters = 1_000_000;
    let segments = 32;
    let segment_size = 1024;
    let key_size = 1;
    let value_size = 512;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);

    let mut rng = rand::rng();

    let mut key = vec![0; key_size];
    let mut value = vec![0; value_size];

    let mut inserts = 0;

    for _ in 0..iters {
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut value);

        if cache.insert(&key, &value, None, ttl).is_ok() {
            inserts += 1;
        };
    }

    assert_eq!(inserts, iters);
}

#[test]
fn full_cache_long_2() {
    let ttl = Duration::ZERO;
    let iters = 10_000_000;
    let segments = 64;
    let segment_size = 2 * 1024;
    let key_size = 2;
    let value_size = 1;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);

    let mut rng = rand::rng();

    let mut key = vec![0; key_size];
    let mut value = vec![0; value_size];

    let mut inserts = 0;

    for _ in 0..iters {
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut value);

        if cache.insert(&key, &value, None, ttl).is_ok() {
            inserts += 1;
        };
    }

    // inserts should be > 99.99 percent successful for this config
    assert!(inserts >= 9_999_000);
}

#[test]
fn expiration() {
    let segments = 64;
    let segment_size = 2 * 1024;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);

    assert!(cache
        .insert(b"latte", b"", None, Duration::from_secs(5))
        .is_ok());
    assert!(cache
        .insert(b"espresso", b"", None, Duration::from_secs(15))
        .is_ok());

    assert!(cache.get(b"latte").is_some());
    assert!(cache.get(b"espresso").is_some());
    assert_eq!(cache.items(), 2);
    assert_eq!(cache.segments.free(), segments - 2);

    // not enough time elapsed, not removed by expire
    cache.expire();
    assert!(cache.get(b"latte").is_some());
    assert!(cache.get(b"espresso").is_some());
    assert_eq!(cache.items(), 2);
    assert_eq!(cache.segments.free(), segments - 2);

    // wait and expire again
    std::thread::sleep(std::time::Duration::from_secs(5));
    cache.expire();

    assert!(cache.get(b"latte").is_none());
    assert!(cache.get(b"espresso").is_some());
    assert_eq!(cache.items(), 1);
    assert_eq!(cache.segments.free(), segments - 1);

    // wait and expire again
    std::thread::sleep(std::time::Duration::from_secs(10));
    cache.expire();

    assert!(cache.get(b"latte").is_none());
    assert!(cache.get(b"espresso").is_none());
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);
}

// Roadmap item 5b, §3: `evict()` must attempt whole-segment expiration
// BEFORE running the spare-consuming Merge eviction. If expiration alone
// frees a segment, merge must not run at all.
//
// Distinguishing signal: Merge's prune step scores items by frequency and
// keeps a target *ratio* of survivors even when every item has the same
// (zero) frequency — so if merge runs on a bucket whose items are all
// past their TTL, some previously-inserted keys will still be readable
// afterward and `cache.items()` will be > 1. Whole-segment expiration, by
// contrast, drops the entire chain unconditionally: every previously
// inserted key becomes `None` and only the newly inserted trigger item
// remains. This is process-local and deterministic (unlike the shared
// `SEGMENT_MERGE` counter, which other tests running in parallel can also
// increment).
//
// Uses `Segments::free_only`, which (like the rest of the Task-1 spare
// accessors) is only compiled outside the `loom` feature.
#[test]
#[cfg(not(feature = "loom"))]
fn evict_expires_before_merging() {
    // Fixed-width key + fixed value so every insert consumes exactly the
    // same number of bytes. `keyvalue::item_size` is the same size
    // formula `reserve_and_define` uses internally, computed here at
    // runtime so the test is correct regardless of ITEM_HDR_SIZE (i.e.
    // under both the default and `integrity`/`debug` feature builds).
    const ITEMS_PER_SEGMENT: usize = 6;
    const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
    let value: &[u8] = b"payload-bytes-value";
    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
    // Under this crate's own `integrity` feature (enabled by `debug`),
    // `Segment::init` writes an 8-byte SEG_MAGIC canary at the start of
    // every segment's data region, shrinking the usable capacity. Fold
    // that into the segment size so ITEMS_PER_SEGMENT items fit exactly
    // regardless of feature flags.
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // 1 held-back spare (Merge policy) + 5 free. Filling all 5 free
    // segments exactly (no partial last segment) forms a chain long
    // enough (chain_len >= 3) that a merge, if it ran, would actually
    // execute rather than bailing out on a too-short chain.
    let free_segments = 5usize;
    let total_segments = free_segments + 1; // + spare

    let cache = Segcache::builder()
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

    assert_eq!(
        cache.segments.free_only(),
        free_segments,
        "sanity: Merge policy must hold back exactly one spare"
    );

    // All inserts share one short TTL so they land in a single TTL
    // bucket's segment chain -- the same chain merge would walk.
    let ttl = Duration::from_secs(1);

    // Fill every normal (non-spare) free segment exactly full with
    // short-TTL items. Because segment_size is an exact multiple of
    // item_size, the last segment ends precisely full too -- eviction is
    // not needed (and must not run) during this fill.
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    let mut inserted = Vec::with_capacity(fill_count);
    for i in 0..fill_count {
        let key = format!("k{i:06}");
        assert_eq!(key.len(), KEY_LEN, "key width must stay fixed-size");
        cache
            .insert(key.as_bytes(), value, None, ttl)
            .expect("fill inserts must succeed without needing eviction");
        inserted.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must exactly exhaust the free queue, including the last segment"
    );

    // Let every inserted item's TTL elapse. clocksource::coarse has 1s
    // resolution, so a >1s real sleep guarantees create_at + ttl <= now
    // for every segment in the chain.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // This insert needs a fresh segment: the pool is genuinely full and
    // the last segment has no spare room. It must be served by evict()
    // reclaiming the whole expired chain via expiration, not by merging
    // it.
    let result = cache.insert(b"trigger", b"new value", None, ttl);
    assert!(
        result.is_ok(),
        "insert must succeed by reclaiming the expired chain"
    );
    assert!(cache.get(b"trigger").is_some());

    // Whole-segment expiration drops every item in the chain -- nothing
    // survives. A merge would instead have pruned by frequency and kept
    // a target *ratio* of survivors alive even though every item here has
    // the same (zero) frequency, so any surviving key below proves merge
    // ran instead of expiration.
    for key in &inserted {
        assert!(
            cache.get(key.as_bytes()).is_none(),
            "key {key} must be gone: expiration (not merge) must have reclaimed the chain"
        );
    }
    assert_eq!(
        cache.items(),
        1,
        "only the trigger item should remain; a merge would have kept survivors"
    );

    // Secondary, non-load-bearing signal: SEGMENT_MERGE is a process-global
    // metriken counter that other tests running in parallel can also
    // increment, so it is deliberately not asserted against a before/after
    // delta here -- the deterministic per-key and items() checks above are
    // what prove the ordering.
    #[cfg(feature = "metrics")]
    let _ = crate::metrics::SEGMENT_MERGE.value();
}

// Roadmap item 5b, §1: the Merge policy evicts by copying survivors into a
// fresh spare segment (reader-safe, append-only) and draining every
// candidate — it never compacts a readable segment in place. This test
// forces a single merge pass over a full segment chain and checks:
//   (a) the bucket head becomes the reserved spare segment, Sealed and
//       holding the copied survivors;
//   (b) the candidate segments were freed and nothing leaked (available +
//       readable == total);
//   (c) high-frequency items survive the merge and are served from their
//       relocated copies in the spare.
//
// Uses the Task-1 spare accessors (`free`, `free_only`, `spare_count`),
// which are compiled only outside the `loom` feature.
#[test]
#[cfg(not(feature = "loom"))]
fn merge_evict_copies_survivors_into_spare() {
    const ITEMS_PER_SEGMENT: usize = 64;
    const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
    let value: &[u8] = b"v";
    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // 1 held-back spare (Merge policy) + 5 normal free segments.
    let free_segments = 5usize;
    let total_segments = free_segments + 1;

    let cache = Segcache::builder()
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

    // Long TTL: items never expire during the test, so evict() falls
    // through the expire-first fast path into the actual merge.
    let ttl = Duration::from_secs(3600);

    // Fill every normal free segment exactly full; all share one TTL bucket
    // (the chain merge walks). The last one stays the Live write tail.
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    let mut keys = Vec::with_capacity(fill_count);
    for i in 0..fill_count {
        let key = format!("k{i:06}");
        assert_eq!(key.len(), KEY_LEN, "key width must stay fixed-size");
        cache
            .insert(key.as_bytes(), value, None, ttl)
            .expect("fill inserts must succeed without needing eviction");
        keys.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must exactly exhaust the free queue"
    );

    // Bump the frequency of a few keys from the first candidate segment so
    // prune keeps them: high-frequency items are the survivors copied into
    // the spare. get() returns a pinned Item, so each lookup is dropped at
    // the end of its statement — no candidate stays pinned during the merge.
    let hot: Vec<String> = keys.iter().take(3).cloned().collect();
    for _ in 0..40 {
        for k in &hot {
            assert!(cache.get(k.as_bytes()).is_some());
        }
    }

    let items_before = cache.items();
    let free_before = cache.segments.free(); // free_only(0) + spare(1)
    assert_eq!(free_before, 1, "only the held-back spare is available");

    // The spare seeded at construction is segment id 1 (idx 0 < spare
    // capacity); reserve_free handed out ids 2.. for the fill, leaving id 1
    // in the spare queue for merge to reserve as the copy destination.
    let spare_id = NonZeroU32::new(1).unwrap();

    // Drive exactly one eviction pass. With a single occupied bucket the
    // policy's random start still finds it (evict scans every bucket).
    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .expect("merge eviction must succeed on a full 5-segment chain");

    // (a) The bucket head is now the spare segment, Sealed and holding the
    // copied survivors.
    let seg_ttl = cache.segments.header(spare_id).ttl();
    let head = cache.ttl_buckets.get_bucket(seg_ttl).head();
    assert_eq!(head, Some(spare_id), "merge must head-insert the spare");
    {
        let spare = cache.segments.segment(spare_id).unwrap();
        assert_eq!(spare.state(), State::Sealed);
        assert!(spare.live_items() > 0, "spare must hold copied survivors");
    }

    // (b) Candidates were drained and nothing leaked. Every candidate that
    // clear_segment recycles becomes Free and is pushed back to a queue
    // (spare first, then free), so the count of Free segments equals the
    // number of drained candidates equals the available depth. At least one
    // candidate must have been drained, and every segment must be accounted
    // for as either available or in a readable chain (no pins are held, so
    // there are no condemned segments).
    let free_after = cache.segments.free();
    assert!(free_after >= free_before, "availability must not shrink");
    let freed = (1..=total_segments as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                == State::Free
        })
        .count();
    assert!(freed >= 1, "merge must have drained at least one candidate");
    assert_eq!(
        free_after, freed,
        "available depth must equal the number of drained (Free) candidates"
    );
    let readable = (1..=total_segments as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        free_after + readable,
        total_segments,
        "no leak: available + readable segments must account for the whole pool"
    );

    // (c) High-frequency items survived and are served from their relocated
    // copies in the spare.
    for k in &hot {
        let item = cache
            .get(k.as_bytes())
            .unwrap_or_else(|| panic!("hot key {k} must survive the merge"));
        assert_eq!(item.value(), b"v");
    }

    // The merge pruned low-frequency items: not every original item can
    // remain (the chain held far more than one spare's worth of survivors).
    assert!(
        cache.items() < items_before,
        "merge must have pruned low-frequency items"
    );
}

// merge_compact is the maintenance counterpart of merge_evict, invoked from
// `remove_at` when a segment drops below the compact-ratio low watermark
// (no full-pool pressure). It must combine under-full segments into a
// fresh spare WITHOUT frequency-based pruning: every survivor from every
// combined candidate is preserved.
//
// Builds a 3-normal-segment (+1 held-back spare) Merge cache, fully fills
// the first two segments (leaving the third as the Live write tail), then
// deletes most items from each so both segments' occupancy lands well
// below the compact ratio (n_compact: 5 => compact_ratio 0.2; each
// candidate ends at 2/12 ≈ 0.167, comfortably clear of the 0.2 watermark
// with margin to spare regardless of any fixed per-segment header
// overhead under the `integrity` feature). The delete that finally drops
// the first segment's ratio to the watermark drives `remove_at` into
// `merge_compact`, which must:
//   (a) reserve the held-back spare and head-insert it as the new Sealed
//       bucket head;
//   (b) copy every survivor from both under-full segments into the spare
//       (no pruning — all survivors preserved, unlike merge_evict);
//   (c) drain both source segments (Free, nothing leaked);
//   (d) leave the untouched Live tail segment alone.
#[test]
#[cfg(not(feature = "loom"))]
fn merge_compact_combines_under_full_segments_into_spare() {
    const ITEMS_PER_SEGMENT: usize = 12;
    const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
    let value: &[u8] = b"v";
    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // 1 held-back spare (Merge policy) + 3 normal free segments: two fill
    // completely and seal, the third stays the Live write tail.
    let free_segments = 3usize;
    let total_segments = free_segments + 1;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 5, // compact_ratio = 1 / 5 = 0.2
        })
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.segments.free_only(), free_segments);
    assert_eq!(cache.segments.spare_count(), 1);

    // Long TTL: items never expire during the test.
    let ttl = Duration::from_secs(3600);

    // Fill exactly 2 segments' worth of items; the reserve() path only
    // seals a segment once a successor is needed, so the 3rd segment
    // stays Live (full, but still the write tail) — mirrors
    // merge_evict_copies_survivors_into_spare's fill discipline.
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    let mut keys = Vec::with_capacity(fill_count);
    for i in 0..fill_count {
        let key = format!("k{i:06}");
        assert_eq!(key.len(), KEY_LEN, "key width must stay fixed-size");
        cache
            .insert(key.as_bytes(), value, None, ttl)
            .expect("fill inserts must succeed without needing eviction");
        keys.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must exactly exhaust the free queue"
    );

    // Deterministic id assignment (same discipline as the merge_evict
    // test): the held-back spare seeded at construction is id 1;
    // reserve_free hands out ids 2.. in order for the fill.
    let spare_id = NonZeroU32::new(1).unwrap();
    let seg_a = NonZeroU32::new(2).unwrap(); // first filled: bucket head, Sealed
    let seg_b = NonZeroU32::new(3).unwrap(); // second filled: Sealed
    let seg_c = NonZeroU32::new(4).unwrap(); // third: Live write tail

    {
        let a = cache.segments.segment(seg_a).unwrap();
        assert_eq!(a.state(), State::Sealed);
        assert_eq!(a.live_items(), ITEMS_PER_SEGMENT as i32);
    }
    {
        let b = cache.segments.segment(seg_b).unwrap();
        assert_eq!(b.state(), State::Sealed);
        assert_eq!(b.live_items(), ITEMS_PER_SEGMENT as i32);
    }
    {
        let c = cache.segments.segment(seg_c).unwrap();
        assert_eq!(c.state(), State::Live);
        assert_eq!(c.live_items(), ITEMS_PER_SEGMENT as i32);
    }

    // Bring seg_b down to 2/12 occupancy FIRST. Its own compact check
    // (`remove_at`) looks at its successor, seg_c — which is Live
    // (can_evict() == false) — so this cannot trigger a merge yet; it's
    // safe prep so that when seg_a's ratio drops, seg_b already qualifies
    // as a compaction partner.
    for k in &keys[12..22] {
        assert!(cache.delete(k.as_bytes()), "delete must find the key");
    }
    assert_eq!(cache.segments.segment(seg_b).unwrap().live_items(), 2);

    // Now bring seg_a down from 12 -> 2 items. Somewhere in this loop
    // seg_a's ratio drops to <= compact_ratio (0.2) while seg_b (its chain
    // successor) is already at 2/12 <= 0.2 and can_evict() == true, so
    // one of these delete() calls drives `remove_at` into
    // `merge_compact(seg_a, ..)`. Once that happens seg_a is drained, so
    // any remaining keys in this batch are deleted from wherever the
    // hashtable now points (harmless — `delete` doesn't care).
    for k in &keys[0..10] {
        assert!(cache.delete(k.as_bytes()), "delete must find the key");
    }

    // (a) The bucket head is now the spare segment, Sealed, and combines
    // both under-full candidates' survivors (2 from seg_a + 2 from
    // seg_b = 4), with none pruned.
    let seg_ttl = cache.segments.header(spare_id).ttl();
    let head = cache.ttl_buckets.get_bucket(seg_ttl).head();
    assert_eq!(
        head,
        Some(spare_id),
        "merge_compact must head-insert the spare"
    );
    {
        let spare = cache.segments.segment(spare_id).unwrap();
        assert_eq!(spare.state(), State::Sealed);
        assert_eq!(
            spare.live_items(),
            4,
            "merge_compact must preserve every survivor from both candidates (no pruning)"
        );
    }

    // (b) Both under-full source segments were drained (Free), and the
    // untouched Live tail was left alone.
    assert_eq!(cache.segments.segment(seg_a).unwrap().state(), State::Free);
    assert_eq!(cache.segments.segment(seg_b).unwrap().state(), State::Free);
    {
        let c = cache.segments.segment(seg_c).unwrap();
        assert_eq!(c.state(), State::Live);
        assert_eq!(c.live_items(), ITEMS_PER_SEGMENT as i32);
    }

    // (c) No leak: available (free + spare) + readable segments accounts
    // for the whole pool.
    let free_after = cache.segments.free();
    let readable = (1..=total_segments as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        free_after + readable,
        total_segments,
        "no leak: available + readable segments must account for the whole pool"
    );

    // (d) Total item count is preserved exactly (4 in the spare + 12 in
    // the untouched Live tail = 16 = 36 inserted - 10 deleted from seg_b's
    // batch - 10 deleted from seg_a's batch).
    assert_eq!(cache.items(), 16);

    // (e) Every surviving key (the ones NOT explicitly deleted) is still
    // reachable, served from wherever the hashtable now points (the
    // relocated copy in the spare, or the untouched tail).
    for k in keys[10..12].iter().chain(&keys[22..36]) {
        let item = cache
            .get(k.as_bytes())
            .unwrap_or_else(|| panic!("surviving key {k} must remain reachable"));
        assert_eq!(item.value(), b"v");
    }

    // (f) The explicitly-deleted keys are gone.
    for k in keys[0..10].iter().chain(&keys[12..22]) {
        assert!(
            cache.get(k.as_bytes()).is_none(),
            "deleted key {k} must not be found"
        );
    }
}

#[test]
fn clear() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);
    assert!(cache.get(b"coffee").is_none());
    assert!(cache.insert(b"coffee", b"strong", None, ttl).is_ok());
    assert_eq!(cache.segments.free(), segments - 1);
    assert_eq!(cache.items(), 1);
    assert!(cache.get(b"coffee").is_some());

    let item = cache.get(b"coffee").unwrap();
    assert_eq!(item.value(), b"strong", "item is: {item:?}");
    // the item pins its segment; release it so clear() can reclaim
    drop(item);

    cache.clear();
    assert_eq!(cache.segments.free(), segments);
    assert_eq!(cache.items(), 0);
    assert!(cache.get(b"coffee").is_none());
}

#[test]
fn wrapping_add() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.insert(b"coffee", 0, None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    assert!(cache.get(b"coffee").is_some());

    let item = cache.get(b"coffee").unwrap();
    assert_eq!(item.value(), 0, "item is: {item:?}");

    // updates are in place: the held Item observes each one
    assert_eq!(cache.wrapping_add(b"coffee", 1).unwrap(), 1);
    assert_eq!(item.value(), 1, "item is: {item:?}");

    // wrap at the 64-bit mark (memcached incr semantics)
    assert_eq!(
        cache.wrapping_add(b"coffee", u64::MAX - 1).unwrap(),
        u64::MAX
    );
    assert_eq!(cache.wrapping_add(b"coffee", 1).unwrap(), 0);
    assert_eq!(cache.wrapping_add(b"coffee", 2).unwrap(), 2);
    assert_eq!(item.value(), 2, "item is: {item:?}");

    // the store agrees
    drop(item);
    assert_eq!(cache.get(b"coffee").unwrap().value(), 2);
}

#[test]
fn saturating_sub() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 64);
    assert!(cache.insert(b"coffee", 3, None, ttl).is_ok());
    assert_eq!(cache.segments.free(), 63);
    assert_eq!(cache.items(), 1);
    assert!(cache.get(b"coffee").is_some());

    let item = cache.get(b"coffee").unwrap();
    assert_eq!(item.value(), 3, "item is: {item:?}");
    drop(item);

    let updated = cache
        .saturating_sub(b"coffee", 2)
        .expect("failed to decrement");
    assert_eq!(updated, 1, "item is: {updated:?}");

    let updated = cache
        .saturating_sub(b"coffee", 1)
        .expect("failed to decrement");
    assert_eq!(updated, 0, "item is: {updated:?}");

    // saturates at zero
    let updated = cache
        .saturating_sub(b"coffee", 1)
        .expect("failed to decrement");
    assert_eq!(updated, 0, "item is: {updated:?}");
    assert_eq!(cache.get(b"coffee").unwrap().value(), 0);
}

#[test]
// This test caught a case where we interpreted old data as part of an item
// header. Specifically, the first insert sets bytes that will be in-range for
// the item header for the third insert. This happens to set the "typed value"
// bit, which stopped the item definition from setting the value length. This
// caused the item value to be invalid. Triggering a removal of this item with
// the corrupted length caused a panic on the asserts, which correctly detected
// the bad state.
fn fuzz_1() {
    let cache = Segcache::builder()
        .segment_size(1024)
        .heap_size(8 * 1024)
        .hash_power(7)
        .overflow_factor(0.0)
        .build()
        .expect("failed to create cache");

    let _ = cache.insert(
        &[
            195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195,
            195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195,
            195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195,
            195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195,
            195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 195, 19, 5, 195,
            195, 195, 195, 195, 195, 195, 195, 195, 4, 0, 4, 2, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
            4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
            4, 4, 4, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 3, 0, 1, 0, 4, 181, 10, 4, 4, 4, 4, 4, 4,
            4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 59, 8, 4,
        ],
        &[4, 4, 4, 4],
        None,
        Duration::from_secs(0),
    );

    let _ = cache.clear();
    assert_eq!(cache.items(), 0);

    let _ = cache.insert(
        &[1],
        &[0xDE, 0xAD, 0xBE, 0xEF],
        None,
        Duration::from_secs(4),
    );
    let _ = cache.insert(&[1], &[0xC0, 0xFF, 0xEE], None, Duration::from_secs(2));
    let _ = cache.delete(&[1]);
}

#[test]
// This test found an issue when freeing a segment because its live item count
// dropped to zero. This is a more complicated way of triggering the same
// behavior as fuzz_1 test, but also exposed that we had a tracking issue for
// dead bytes when recycling a segment when live items dropped to zero.
fn fuzz_2() {
    let cache = Segcache::builder()
        .segment_size(1024)
        .heap_size(8 * 1024)
        .hash_power(7)
        .overflow_factor(1.0)
        .build()
        .expect("failed to create cache");

    let _ = cache.insert(&[1], &[3, 4, 2], None, Duration::from_secs(0));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 3, 0], None, Duration::from_secs(4));
    let _ = cache.insert(&[1], &[3, 4, 2], None, Duration::from_secs(0));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[2], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1,
        ],
        &[],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(&[1], &[], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            228, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 18, 255, 1, 0, 0, 1, 0, 2,
            4, 1, 1, 1, 1, 1, 1, 1, 1, 101, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3,
            0, 1, 0, 4, 2, 114, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 0, 4, 0, 4, 48, 0, 0, 255, 1, 0, 0, 1, 0, 2, 4, 1, 1, 1, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0,
        ],
        &[1],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[3, 4, 0], &[3, 1, 0], None, Duration::from_secs(10));
    let _ = cache.delete(&[3, 1, 0]);
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1,
        ],
        &[],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(
        &[
            228, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 2, 1, 3, 0, 1, 3, 0, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4,
            0, 1, 3, 0, 4, 1, 0, 81, 0, 0, 0, 1, 0, 10, 81, 0, 0, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1,
            1, 0, 1, 3, 4, 2, 116, 0, 2, 2, 255, 255, 0, 3, 1, 0, 2, 0, 0, 0, 3, 4, 10, 4, 2, 114,
            0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 5, 0, 255, 252, 255, 254, 255, 251, 2, 114, 0, 4, 4,
            4, 0, 1, 1, 2, 1, 1, 0, 1, 1, 2, 1, 1, 0, 1, 2, 1, 1, 0, 1, 1, 1, 0, 1, 1, 2, 1, 1, 0,
            1, 1, 1, 0, 1, 0, 4, 48, 0, 0, 3, 4, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ],
        &[
            255, 255, 255, 255, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4, 0, 1, 3, 0,
            4, 1, 0, 81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 255, 255, 255, 255, 0, 4, 0, 4, 48,
            0, 0, 255, 1, 0, 0, 1, 0, 2, 4, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 4, 2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4,
            10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4, 0, 1, 3, 0, 4, 1, 0, 81, 0, 0,
            0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0,
            0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1, 0, 0, 228, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48,
        ],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 4, 2], None, Duration::from_secs(114));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[3], &[3, 4, 2], None, Duration::from_secs(114));
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[2], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[3, 4, 0], &[3, 1, 0], None, Duration::from_secs(10));
    let _ = cache.delete(&[3, 1, 0]);
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1,
        ],
        &[],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(
        &[
            228, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4,
            0, 1, 3, 0, 4, 1, 0, 81, 0, 0, 0, 1, 0, 10, 81, 0, 0, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1,
            1, 0, 1, 3, 4, 2, 116, 0, 2, 2, 255, 255, 0, 3, 1, 0, 2, 0, 0, 0, 3, 4, 10, 4, 2, 114,
            0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0,
            0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4, 0, 1, 3, 0, 4, 1, 0,
            81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1,
        ],
        &[3, 0, 1],
        None,
        Duration::from_secs(3),
    );
    let _ = cache.insert(&[1], &[], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            228, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 18, 255, 1, 0, 0, 1, 0, 2,
            4, 1, 1, 1, 1, 1, 1, 1, 1, 101, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3,
            0, 1, 0, 4, 2, 114, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 0, 4, 0, 4, 48, 0, 0, 255, 1, 0, 0, 1, 0, 2, 4, 1, 1, 1, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0,
        ],
        &[1],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[3, 4, 0], &[3, 1, 0], None, Duration::from_secs(10));
    let _ = cache.delete(&[3, 1, 0]);
    let _ = cache.insert(&[4, 0, 4, 48], &[], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(
        &[
            81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1,
        ],
        &[],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(
        &[
            228, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 2, 1, 3, 0, 1, 3, 0, 1, 0, 4,
            2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4,
            0, 1, 3, 0, 4, 1, 0, 81, 0, 0, 0, 1, 0, 10, 81, 0, 0, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1,
            1, 0, 1, 3, 4, 2, 116, 0, 2, 2, 255, 255, 0, 3, 1, 0, 2, 0, 0, 0, 3, 4, 10, 4, 2, 114,
            0, 4, 0, 4, 48, 0, 0, 3, 4, 10, 1, 5, 0, 255, 252, 255, 254, 255, 251, 2, 114, 0, 4, 4,
            4, 0, 1, 1, 2, 1, 1, 0, 1, 1, 2, 1, 1, 0, 1, 2, 1, 1, 0, 1, 1, 1, 0, 1, 1, 2, 1, 1, 0,
            1, 1, 1, 0, 1, 0, 4, 48, 0, 0, 3, 4, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ],
        &[
            255, 255, 255, 255, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4, 0, 1, 3, 0,
            4, 1, 0, 81, 0, 0, 0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 255, 255, 255, 255, 0, 4, 0, 4, 48,
            0, 0, 255, 1, 0, 0, 1, 0, 2, 4, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
            2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 4, 2, 1, 0, 1, 3, 4, 2, 114, 0, 4, 0,
            4, 48, 0, 0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0, 0, 3, 4,
            10, 1, 3, 0, 1, 3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 3, 0, 4, 0, 1, 3, 0, 4, 1, 0, 81, 0, 0,
            0, 1, 0, 10, 0, 1, 3, 0, 1, 0, 1, 0, 3, 0, 1, 3, 0, 1, 0, 4, 2, 114, 0, 4, 0, 4, 48, 0,
            0, 3, 4, 10, 1, 3, 0, 1, 3, 0, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 228, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237,
            237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 237, 1, 0, 0, 228, 3, 0, 1, 0, 4, 2,
            114, 0, 4, 0, 4, 48,
        ],
        None,
        Duration::from_secs(0),
    );
    let _ = cache.insert(&[1], &[3, 0, 1], None, Duration::from_secs(0));
    let _ = cache.insert(&[1], &[3, 4, 2], None, Duration::from_secs(114));
}

// Roadmap item 7b: `get`/`get_no_freq_incr` are `&self` so N threads can
// share `&Segcache` for genuinely concurrent reads. Populate a cache with
// known key -> value pairs (&mut phase), then share &cache across threads
// for a read-only concurrent phase. No writes happen during the concurrent
// phase, so every present key has a fixed value -- any torn read, corrupted
// freq slot, or botched pin surfaces as a wrong value or a crash.
//
// This also doubles as the compile-time consumer of the Task-1 `Segcache:
// Sync` guard: the test would not compile if `Segcache` were `!Sync`, since
// `std::thread::scope` requires the captured `&Segcache` to be `Sync` to
// share it across spawned threads.
#[test]
fn concurrent_readers_see_correct_values() {
    const KEYS: usize = 500;
    const THREADS: usize = 8;
    const ROUNDS: usize = 4_000;

    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("build cache");

    let key = |i: usize| format!("k{i:06}").into_bytes();
    let val = |i: usize| format!("val-{i:06}").into_bytes();
    for i in 0..KEYS {
        cache
            .insert(&key(i), val(i).as_slice(), None, Duration::ZERO)
            .expect("insert");
    }

    // Sanity: all present before the concurrent phase (no eviction happened).
    for i in 0..KEYS {
        let item = cache.get(&key(i)).expect("present");
        assert!(item.value() == *val(i).as_slice());
    }

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let cache = &cache; // shared &Segcache -- requires Segcache: Sync
            s.spawn(move || {
                for r in 0..ROUNDS {
                    let i = (t * 31 + r * 17) % KEYS;
                    // Hold two pins at once to exercise overlapping ref_counts.
                    let a = cache.get(&key(i)).expect("present key must be found");
                    assert!(
                        a.value() == *val(i).as_slice(),
                        "torn/wrong value for key {i}"
                    );

                    let j = (i + 7) % KEYS;
                    let b = cache.get_no_freq_incr(&key(j)).expect("present");
                    assert!(b.value() == *val(j).as_slice());

                    assert!(cache.get(b"definitely-absent-key").is_none());

                    drop(a);
                    drop(b);
                }
            });
        }
    });

    // No reader pin leaked: every segment's ref_count is back to 0. Pins are
    // only guaranteed released once all reader threads have joined above.
    for id in 1..=segments as u32 {
        assert_eq!(
            cache
                .segments
                .header(NonZeroU32::new(id).unwrap())
                .ref_count(),
            0,
            "segment {id} has a leaked reader pin",
        );
    }

    // After joining: cache still serves, and a write still works (exclusive &mut).
    for i in 0..KEYS {
        let item = cache
            .get(&key(i))
            .expect("still present after concurrent reads");
        assert!(item.value() == *val(i).as_slice());
    }
    cache
        .insert(b"post", b"ok", None, Duration::ZERO)
        .expect("insert after concurrent reads");
    assert!(cache.get(b"post").unwrap().value() == *b"ok".as_slice());
}

// Item 7d: every reserve pins its segment (WriterPin, carried by
// ReservedItem); every write must RELEASE that pin before returning. Run the
// real public write paths — insert (fresh and replace), cas, and delete — and
// assert no segment is left with a stuck writer pin afterward.
//
// Scope: this is a LEAK check, not an ordering check. Single-threaded and
// `&mut`, with Rust's drop-at-end-of-scope, it catches a pin that is never
// released — forgotten (`mem::forget`), stashed somewhere that outlives the
// call, or dropped on a path that skips the release. It CANNOT distinguish a
// pin dropped just before publish from one dropped just after: both leave
// `active_writers == 0` by the time this samples the quiesced end state. The
// H2 ordering guarantee (the pin actually SPANS publish, so a racing drain
// can't recycle mid-write) is enforced by the concurrent reserver-vs-drain
// test in `eviction_concurrency_tests.rs`, which has a real racing thread.
#[test]
fn writer_pins_released_after_write_ops() {
    let cache = Segcache::builder().build().expect("failed to create cache");

    cache
        .insert(b"k1", b"v1", None, Duration::from_secs(60))
        .unwrap();
    cache
        .insert(b"k1", b"v2", None, Duration::from_secs(60))
        .unwrap(); // replace path
    let cur = cache.get(b"k1").unwrap().cas();
    cache
        .cas(b"k1", b"v3", None, Duration::from_secs(60), cur)
        .unwrap(); // cas path
    cache.delete(b"k1");

    // Every reserve pinned its segment; every publish/rollback path must have
    // released it. No segment may retain a writer pin once the calls return
    // (item 7d, H2: the pin spans publish, then drops).
    for h in cache.segments_for_test().iter_headers_for_test() {
        assert_eq!(h.active_writers(), 0, "leaked writer pin after write ops");
    }
}
