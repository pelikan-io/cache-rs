// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Guards for the incarnation tag that a `Location` carries.
//!
//! Two distinct failure shapes are covered here, both of which are SILENT
//! without a dedicated test:
//!
//! 1. **The reconstruction sites.** `Segment::copy_into` (merge) and
//!    `Segments::s3fifo_promote_from` (S3-FIFO promotion) rebuild the location
//!    an item was published under, in order to CAS against it. If that
//!    reconstruction cannot reproduce the published tag, every relink CAS fails
//!    and relocation degrades to a complete no-op: nothing errors, no counter
//!    goes negative, no integrity check trips — the cache simply stops moving
//!    items and quietly drops what it should have kept. The two tests here
//!    assert the relocation ITSELF (items observed at new locations in the
//!    destination segment), not merely that the cache still answers queries,
//!    so each site has a test that reddens when its generation argument is
//!    wrong. Verified by neutering each site in turn.
//!
//! 2. **The stale-location policy.** A location whose tag no longer matches its
//!    segment's live generation names an item that no longer exists.
//!    `Segments::resolve` rejects it, and each consumer answers per the design's
//!    policy table: a lookup treats it as a miss, `acquire_item_at` refuses the
//!    pin, `remove_at` skips the decrement, and `Segcache::replace_at` refuses
//!    to address the item at all (rolling its reservation back and reporting
//!    `Exists`, its ordinary lost-the-race answer). None of them is an error
//!    path.

use crate::*;
use core::num::NonZeroU32;
use std::time::Duration;

const KEY_LEN: usize = 7;
const VAL_LEN: usize = 7;

/// Compare an item's value against expected bytes (`Value` is an enum, so a
/// numeric item must fail loudly rather than silently compare unequal).
fn assert_value_eq(v: Value, expected: &[u8], msg: &str) {
    match v {
        Value::Bytes(b) => assert_eq!(b, expected, "{msg}"),
        other => panic!("{msg}: expected bytes {expected:?}, got {other:?}"),
    }
}

fn key_of(i: usize) -> String {
    format!("k{i:06}")
}

fn val_of(i: usize) -> String {
    format!("v{i:06}")
}

/// Bytes of a segment consumed by the leading magic word, when built with the
/// `integrity` feature.
fn magic_overhead() -> usize {
    if cfg!(feature = "integrity") {
        8
    } else {
        0
    }
}

/// Segment size that holds exactly `items` fixed-width items.
fn segment_size_for(items: usize) -> i32 {
    let sample = val_of(0);
    assert_eq!(sample.len(), VAL_LEN);
    assert_eq!(key_of(0).len(), KEY_LEN);
    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample.as_bytes()), 0);
    (magic_overhead() + item_size * items) as i32
}

/// The location a key currently resolves to, or `None` if it is not in the
/// hashtable. Read WITHOUT bumping frequency, so calling it cannot change which
/// items an eviction pass decides to keep.
fn location_of(cache: &Segcache, key: &[u8]) -> Option<Location> {
    let verifier = cache.segments.verifier();
    cache
        .hashtable
        .lookup_no_freq_update(key, &verifier)
        .map(|(location, _freq)| location)
}

/// The segment a location addresses, ignoring its tag (so a stale location can
/// still be pointed at its segment for assertions).
fn segment_of(location: Location) -> NonZeroU32 {
    let (seg_id, _offset) = unpack_location(location);
    NonZeroU32::new(seg_id).expect("a published location names a real segment")
}

/// S3-FIFO promotion must actually MOVE items out of the admission pool into a
/// main-pool segment and relink them there.
///
/// This is the site with no other coverage: `s3fifo_promote_from`'s
/// reconstruction feeds `get_item_frequency`, so a wrong tag makes every item
/// look frequency-zero, promotion copies nothing, and the whole admission
/// segment is dropped instead. The cache stays consistent, every other test
/// stays green, and S3-FIFO silently becomes FIFO-with-extra-steps.
///
/// So the assertions are about the relocation itself: the survivors must be
/// found at NEW locations, in a segment of the MAIN pool, resolvable there
/// (the tag they were re-published under matching the destination's
/// generation), carrying their own values — and the destination's live-item
/// count must equal the number promoted.
#[test]
fn s3fifo_promotion_relocates_items_into_the_main_pool() {
    const ITEMS_PER_SEGMENT: usize = 8;
    // A destination fills to `write_offset + item_size < segment_size`, so the
    // last item of a full source segment has nowhere to go and is dropped.
    const PROMOTABLE_PER_SEGMENT: usize = ITEMS_PER_SEGMENT - 1;
    const TOTAL_SEGMENTS: usize = 16;

    let segment_size = segment_size_for(ITEMS_PER_SEGMENT);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        // admission_cap = round(16 * 0.25) = 4 segments, so the fill below
        // never trips the pool-full self-eviction: this test drives exactly
        // one eviction pass, itself.
        .eviction(Policy::S3Fifo {
            admission_ratio: 0.25,
        })
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);

    // Fill two admission segments completely (they seal) plus one item into a
    // third, which stays the Live tail and is therefore not an eviction
    // candidate.
    let filled = 2 * ITEMS_PER_SEGMENT;
    for i in 0..=filled {
        cache
            .insert(key_of(i).as_bytes(), val_of(i).as_bytes(), None, ttl)
            .expect("fill inserts must succeed without needing eviction");
    }

    // Warm every filled key. A fresh entry is published at frequency 1, so all
    // of them are already promotion-eligible; the reads make the intent
    // explicit and keep the test honest if that initial value ever changes.
    // Which of the two sealed segments the policy drains depends on
    // coarse-clock creation times, so both are prepared identically and the
    // assertions below follow whichever was chosen.
    for _ in 0..8 {
        for i in 0..filled {
            assert!(
                cache.get(key_of(i).as_bytes()).is_some(),
                "filled key {i} must be present before the eviction pass"
            );
        }
    }

    // Snapshot where everything lives, and confirm the premise: the fill is in
    // the admission pool.
    let before: Vec<Option<Location>> = (0..filled)
        .map(|i| location_of(&cache, key_of(i).as_bytes()))
        .collect();
    for (i, loc) in before.iter().enumerate() {
        let loc = loc.unwrap_or_else(|| panic!("fill key {i} must be published"));
        assert_eq!(
            cache.segments.header(segment_of(loc)).pool(),
            SegmentPool::Admission,
            "fill key {i} must start in the admission pool"
        );
    }

    // Exactly one eviction pass: admission-pool first, so this is
    // s3fifo_evict_admission -> s3fifo_promote_from.
    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .expect("an admission-pool eviction pass must succeed");

    // Collect what moved. A key that is gone was dropped; a key at its
    // original location was in the segment this pass did not touch.
    let mut promoted = Vec::new();
    let mut destination: Option<NonZeroU32> = None;
    for (i, old) in before.iter().enumerate() {
        let old = old.expect("key was published before the pass");
        let Some(new) = location_of(&cache, key_of(i).as_bytes()) else {
            continue; // dropped (did not fit in the destination)
        };
        if new == old {
            continue; // untouched: still in the admission pool
        }
        promoted.push(i);

        let dst = segment_of(new);
        assert_ne!(
            dst,
            segment_of(old),
            "a promoted item must live in a DIFFERENT segment than it was copied from"
        );
        assert_eq!(
            cache.segments.header(dst).pool(),
            SegmentPool::Main,
            "promotion must land in the MAIN pool"
        );
        assert_eq!(
            *destination.get_or_insert(dst),
            dst,
            "all promotions of one pass share one destination segment"
        );
        // The new location is resolvable: its tag matches the destination's
        // live generation, which is what makes it addressable at all.
        assert_eq!(
            cache.segments.resolve(new),
            Some((dst, unpack_location(new).1)),
            "the re-published location must resolve to the destination"
        );
        // And it really is this key's item there.
        let item = cache
            .get(key_of(i).as_bytes())
            .unwrap_or_else(|| panic!("promoted key {i} must be readable at its new location"));
        assert_value_eq(
            item.value(),
            val_of(i).as_bytes(),
            &format!("promoted key {i} must carry its own value"),
        );
    }

    // The headline assertion: promotion happened at all, for every item that
    // fits. A neutered reconstruction leaves this EMPTY — every item looks
    // frequency-zero, nothing is copied, and the source is dropped wholesale.
    assert_eq!(
        promoted.len(),
        PROMOTABLE_PER_SEGMENT,
        "every item of the drained admission segment that fits must promote; promoted {promoted:?}"
    );

    let destination = destination.expect("a promotion destination must exist");
    let dst_seg = cache.segments.segment(destination).unwrap();
    assert_eq!(
        dst_seg.live_items() as usize,
        promoted.len(),
        "the destination must account for exactly the promoted items"
    );

    // The source segment was drained and freed, and the one item that did not
    // fit is gone.
    let source = segment_of(before[promoted[0]].unwrap());
    assert_eq!(
        cache.segments.header(source).state(),
        State::Free,
        "the drained admission segment must have been recycled"
    );
    let from_source: Vec<usize> = (0..filled)
        .filter(|i| segment_of(before[*i].unwrap()) == source)
        .collect();
    assert_eq!(
        from_source.len(),
        ITEMS_PER_SEGMENT,
        "the drained segment held a full complement of items"
    );
    for i in from_source {
        if promoted.contains(&i) {
            continue;
        }
        assert!(
            cache.get(key_of(i).as_bytes()).is_none(),
            "key {i} was not promoted, so it must not survive its segment's drain"
        );
    }
}

/// Merge eviction must actually MOVE surviving items into the copy destination
/// and relink them there.
///
/// The named guard for `Segment::copy_into`'s reconstruction, the sibling of
/// the S3-FIFO test above and the same failure shape: a wrong tag makes every
/// item look deleted, the merge copies nothing, and the survivors are dropped.
#[test]
fn merge_relocates_survivors_into_the_spare() {
    const ITEMS_PER_SEGMENT: usize = 16;
    const HOT: usize = 3;
    const FREE_SEGMENTS: usize = 5;

    let segment_size = segment_size_for(ITEMS_PER_SEGMENT);
    // One held-back spare (Merge policy) plus the normal free segments.
    let total_segments = FREE_SEGMENTS + 1;

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

    let ttl = Duration::from_secs(3600);

    let fill = ITEMS_PER_SEGMENT * FREE_SEGMENTS;
    for i in 0..fill {
        cache
            .insert(key_of(i).as_bytes(), val_of(i).as_bytes(), None, ttl)
            .expect("fill inserts must succeed without needing eviction");
    }

    // Make the first items hot so the merge's prune keeps them: these are the
    // survivors that must be copied forward.
    for _ in 0..40 {
        for i in 0..HOT {
            assert!(cache.get(key_of(i).as_bytes()).is_some());
        }
    }

    let before: Vec<Location> = (0..HOT)
        .map(|i| location_of(&cache, key_of(i).as_bytes()).expect("hot key must be published"))
        .collect();

    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .expect("merge eviction must succeed on a full chain");

    let mut destination: Option<NonZeroU32> = None;
    for (i, &old) in before.iter().enumerate() {
        let new = location_of(&cache, key_of(i).as_bytes())
            .unwrap_or_else(|| panic!("hot key {i} must survive the merge"));
        assert_ne!(
            new, old,
            "hot key {i} must have been RELOCATED, not left behind in a drained segment"
        );

        let dst = segment_of(new);
        assert_ne!(
            dst,
            segment_of(old),
            "the copy destination must be a new segment"
        );
        assert_eq!(
            *destination.get_or_insert(dst),
            dst,
            "one merge pass copies into one destination"
        );
        assert_eq!(
            cache.segments.resolve(new),
            Some((dst, unpack_location(new).1)),
            "the relinked location must resolve to the destination"
        );
        let item = cache
            .get(key_of(i).as_bytes())
            .unwrap_or_else(|| panic!("hot key {i} must be readable at its new location"));
        assert_value_eq(
            item.value(),
            val_of(i).as_bytes(),
            &format!("relocated key {i} must carry its own value"),
        );
    }

    let destination = destination.expect("a merge destination must exist");
    assert!(
        cache.segments.segment(destination).unwrap().live_items() as usize >= HOT,
        "the destination must hold the copied survivors"
    );

    // The source was recycled, so the locations the survivors USED to have are
    // now stale: `resolve` rejects them. This is the same fact the relink CAS
    // relies on, observed from the outside.
    let source = segment_of(before[0]);
    assert_eq!(
        cache.segments.header(source).state(),
        State::Free,
        "the merged-away source must have been recycled"
    );
    assert_eq!(
        cache.segments.resolve(before[0]),
        None,
        "a location into a recycled segment must no longer resolve"
    );
}

/// A location whose segment has been recycled is rejected everywhere, and each
/// consumer answers with its no-op-shaped policy rather than an error.
///
/// The segment is deliberately REFILLED before the assertions, so every
/// rejection below is attributable to the incarnation tag and not to the
/// segment merely being unreadable: a freshly published location into the very
/// same segment is accepted in the same breath.
#[test]
fn stale_location_is_rejected_by_every_consumer() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const TOTAL_SEGMENTS: usize = 4;

    let segment_size = segment_size_for(ITEMS_PER_SEGMENT);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        .eviction(Policy::Random)
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);
    cache
        .insert(key_of(0).as_bytes(), val_of(0).as_bytes(), None, ttl)
        .expect("first insert must succeed");

    let stale = location_of(&cache, key_of(0).as_bytes()).expect("key must be published");
    let seg = segment_of(stale);
    let generation_before = cache.segments.generation(seg);

    // Recycle the segment: clear() drains every segment, and the transition
    // that ends a used incarnation bumps the generation.
    cache.clear();
    assert_ne!(
        cache.segments.generation(seg),
        generation_before,
        "recycling a used segment must advance its generation"
    );

    // Refill until the segment is in use again, so it is readable and pinnable
    // — the rejections below then isolate the tag.
    let mut fresh_key = None;
    for i in 1..(ITEMS_PER_SEGMENT * TOTAL_SEGMENTS) {
        cache
            .insert(key_of(i).as_bytes(), val_of(i).as_bytes(), None, ttl)
            .expect("refill inserts must succeed");
        if let Some(loc) = location_of(&cache, key_of(i).as_bytes()) {
            if segment_of(loc) == seg {
                fresh_key = Some(i);
                break;
            }
        }
    }
    let fresh_key = fresh_key.expect("the recycled segment must be handed out again");
    let fresh = location_of(&cache, key_of(fresh_key).as_bytes()).unwrap();
    assert!(
        cache.segments.header(seg).state().is_readable(),
        "the segment must be live again, so the assertions below isolate the tag"
    );

    // (1) `resolve` rejects the stale location and accepts a fresh one into the
    //     very same segment.
    assert_eq!(
        cache.segments.resolve(stale),
        None,
        "a location from a previous incarnation must not resolve"
    );
    assert!(
        cache.segments.resolve(fresh).is_some(),
        "the current incarnation's own locations must still resolve"
    );

    // (2) `acquire_item_at` fails the pin for the stale location, and grants it
    //     for the fresh one.
    assert!(
        cache.segments.acquire_item_at(stale).is_none(),
        "a stale location must not be pinnable"
    );
    assert!(
        cache.segments.acquire_item_at(fresh).is_some(),
        "the same segment must still be pinnable through a current location"
    );

    // (3) `remove_at` skips the decrement. Plant the stale location on a
    //     pinned segment and assert the incarnation's accounting is untouched —
    //     the counters belong to the live incarnation and were reset wholesale
    //     when it was reserved.
    let (live_items, live_bytes) = {
        let s = cache.segments.segment(seg).unwrap();
        (s.live_items(), s.live_bytes())
    };
    let pin = cache
        .segments
        .try_pin_remover(seg)
        .expect("a readable segment must be pinnable for removal");
    cache
        .segments
        .remove_at(stale, &cache.ttl_buckets, &cache.hashtable, pin)
        .expect("a stale location is skipped, never reported as an error");
    let s = cache.segments.segment(seg).unwrap();
    assert_eq!(
        (s.live_items(), s.live_bytes()),
        (live_items, live_bytes),
        "a stale remove must not decrement the live incarnation's accounting"
    );

    // (4) A lookup that returns a stale location is a MISS. Plant one for a key
    //     whose bytes really are at that address — so the hashtable's own key
    //     verification passes and only the tag can reject it — and confirm the
    //     read reports the key gone rather than serving the current occupant
    //     through a location that no longer names it.
    let stale_fresh = pack_location(
        seg,
        cache.segments.generation(seg).wrapping_sub(1),
        unpack_location(fresh).1 as u64,
    );
    assert!(
        cache
            .hashtable
            .cas_location(key_of(fresh_key).as_bytes(), fresh, stale_fresh, true),
        "planting the stale entry must succeed"
    );
    assert!(
        cache.get(key_of(fresh_key).as_bytes()).is_none(),
        "a lookup resolving to a stale incarnation must report a miss"
    );

    // (5) `replace_at` — the cas / try_into_numeric publish path — refuses to
    //     address the stale location, rolls its reservation back and reports
    //     `Exists`.
    //
    //     Why this needs its own guard: `replace_at` takes a REMOVER PIN on the
    //     old item's segment and then, on the cas path, builds a `RawItem` at
    //     the location's offset to re-verify the caller's token under that pin.
    //     The pin freezes the generation from the moment it is taken, but it
    //     does not prove the segment it froze is the incarnation the location
    //     names — and the presence check it does run (`get_item_frequency`)
    //     matches on (tag, location) alone, so a slot still carrying a
    //     stale-tagged location reports present. Without an explicit `resolve`
    //     under the pin, the offset would be dereferenced inside a DIFFERENT
    //     incarnation, where it need not even be an item boundary: a garbage
    //     `is_numeric` bit reading true makes the seqlock acquire CAS a version
    //     word into another incarnation's live payload.
    //
    //     The token passed in is exactly the one the publish path recomputes
    //     (the stale location plus the segment's CURRENT generation), so the
    //     token compare CANNOT be what rejects this call — only the incarnation
    //     gate can. The stale entry planted in (4) is still in place, and the
    //     bytes it addresses really are this key's, so the hashtable's own key
    //     verification passes too.
    let planted = cache
        .hashtable
        .lookup_slot(key_of(fresh_key).as_bytes(), &cache.segments.verifier())
        .expect("the planted stale entry is still found by key bytes");
    assert_eq!(
        planted.0, stale_fresh,
        "the planted stale location is what the publish path would be handed"
    );
    let generation = cache.segments.generation(seg);
    let token = crate::cas::CasToken::new(stale_fresh, generation).as_raw();
    assert_eq!(
        cache.replace_at_for_test(
            key_of(fresh_key).as_bytes(),
            stale_fresh,
            planted.1,
            val_of(fresh_key).as_bytes(),
            Some(token),
        ),
        Err(SegcacheError::Exists),
        "a publish against a location from a previous incarnation must be refused"
    );
    assert_eq!(
        cache.segments.generation(seg),
        generation,
        "the reservation must not have recycled the segment under the test \
         (otherwise the rejection above proves nothing about the tag)"
    );
    assert_eq!(
        location_of(&cache, key_of(fresh_key).as_bytes()),
        Some(stale_fresh),
        "a refused publish must leave the hashtable entry exactly as it found it"
    );

    // Restore the real entry; the key is readable again, proving the miss above
    // was the tag talking and nothing else.
    assert!(
        cache
            .hashtable
            .cas_location(key_of(fresh_key).as_bytes(), stale_fresh, fresh, true),
        "restoring the live entry must succeed"
    );
    assert!(
        cache.get(key_of(fresh_key).as_bytes()).is_some(),
        "the key must be readable again once its live location is restored"
    );
}
