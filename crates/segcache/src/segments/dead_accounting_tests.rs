//! Per-segment dead-space accounting: a RELOCATION is not a death.
//!
//! `ITEM_DEAD`/`ITEM_DEAD_BYTES` are occupancy gauges — dead weight currently
//! sitting in segments — maintained in lockstep with the per-segment
//! `dead_items`/`dead_bytes` header counters that `SegmentHeader` carries:
//! `Segment::remove_item_at` charges an item's space to its segment, and
//! `SegmentHeader::reset_write_stats` reclaims the whole charge when the
//! segment is recycled, re-reserved, or freed on the condemned path (by
//! whichever of the three claimants wins the AwaitingRelease -> Free CAS).
//!
//! Both relocation sites (`Segment::copy_into` for merge drains,
//! `Segments::s3fifo_promote_from` for S3-FIFO promotions) run
//! `remove_item_at` on the SOURCE while the item lives on in the destination,
//! so each has to take the dead charge straight back off.
//!
//! WHY THESE ASSERT ON HEADER COUNTERS, NOT THE GLOBAL GAUGES. Two reasons.
//! (1) The gauges are process-global statics and libtest runs this binary's
//! tests concurrently, so an absolute assertion on them from a unit test is
//! inherently racy (that is why `tests/item_gauges.rs` and friends are
//! one-test-per-binary). (2) More importantly, relocation inflation is NOT
//! observable in the global gauges at any quiescent point once the reclaim at
//! recycle exists: a merge source is drained and recycled microseconds after
//! the copy, and the reclaim subtracts exactly whatever was charged — a wrong
//! charge and its reclaim cancel. The per-segment counter, inspected while the
//! source is still mid-drain, is where the difference is visible at all.

use super::*;
use crate::eviction::Policy;
use crate::Segcache;
use core::num::NonZeroU32;
use keyvalue::Value;
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
const VAL_LEN: usize = 7; // "V" + 6 zero-padded digits
/// Enough segments that a two-segment fill never triggers eviction, under
/// either policy (S3-FIFO's admission pool is a fraction of the total).
const TOTAL_SEGMENTS: usize = 12;

fn key_of(i: usize) -> String {
    format!("k{i:06}")
}

fn val_of(i: usize) -> String {
    format!("V{i:06}")
}

fn item_size() -> usize {
    let sample = val_of(0);
    assert_eq!(sample.len(), VAL_LEN);
    keyvalue::item_size(KEY_LEN, &Value::Bytes(sample.as_bytes()), 0)
}

fn cache_with(policy: Policy, total_segments: usize) -> Segcache {
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size() * ITEMS_PER_SEGMENT) as i32;

    Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(policy)
        .build()
        .expect("failed to create cache")
}

/// The one `Sealed` segment the fill below leaves behind — the filled segment
/// that is no longer the write tail. Found by scan rather than by assuming an
/// id allocation order or a per-policy layout (S3-FIFO splits the fill across
/// its admission and main pools, so neither the id nor the exact occupancy is
/// the same as under Merge).
fn sealed_segment(cache: &Segcache) -> NonZeroU32 {
    let mut found = None;
    for raw in 1..=TOTAL_SEGMENTS as u32 {
        let id = NonZeroU32::new(raw).unwrap();
        let header = cache.segments.header(id);
        if header.state() == State::Sealed && header.live_items() > 0 {
            assert!(found.is_none(), "more than one sealed segment");
            found = Some(id);
        }
    }
    found.expect("no sealed segment — the fill did not lay out as expected")
}

/// Fill two segments' worth of items: one seals, the other becomes the Live
/// write tail. Returns the id of the sealed one.
fn fill_two_segments(cache: &Segcache, ttl: Duration) -> NonZeroU32 {
    for i in 0..2 * ITEMS_PER_SEGMENT {
        cache
            .insert(key_of(i).as_bytes(), val_of(i).as_bytes(), None, ttl)
            .expect("fill insert must succeed without needing eviction");
    }
    let src = sealed_segment(cache);
    assert_eq!(
        cache.segments.header(src).dead_items(),
        0,
        "a freshly filled segment holds no dead space"
    );
    src
}

/// Delete keys until `target` of the SOURCE segment's items have died, and
/// return the dead charge that must now be on it. Deleting is the plainest
/// real death (`remove_at` -> `remove_item_at`); which key lands in which
/// segment is a layout detail, so this drives the counter rather than
/// predicting the mapping.
fn delete_until_dead(cache: &Segcache, src: NonZeroU32, target: i32) -> (i32, i32) {
    for i in 0..2 * ITEMS_PER_SEGMENT {
        if cache.segments.header(src).dead_items() >= target {
            break;
        }
        let _ = cache.delete(key_of(i).as_bytes());
    }
    let dead_items = cache.segments.header(src).dead_items();
    assert_eq!(
        dead_items, target,
        "could not stage exactly {target} real deaths in the source segment"
    );
    (dead_items, dead_items * item_size() as i32)
}

/// A merge drain's copy-out must leave the source's dead charge exactly where
/// the real deaths left it: the survivors it moves are relocated, not killed.
///
/// Neutering check (how this test earns its keep): drop the
/// `self.header.decr_dead_item(...)` line from `Segment::copy_into` and the
/// source's dead total jumps from the two deleted items to every item it ever
/// held (measured: 8 items / 256 bytes against an expected 2 / 64), failing
/// the assertion below.
#[test]
fn copy_into_does_not_charge_the_source_with_dead_space() {
    let cache = cache_with(
        Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        },
        TOTAL_SEGMENTS,
    );
    let ttl = Duration::from_secs(3600);

    let src_id = fill_two_segments(&cache, ttl);

    // Two real deaths in the source; whatever is left is relocated.
    let (dead_items, dead_bytes) = delete_until_dead(&cache, src_id, 2);
    assert_eq!(
        cache.segments.header(src_id).dead_bytes(),
        dead_bytes,
        "a delete must charge its item's space to the segment"
    );
    let survivors = cache.segments.header(src_id).live_items();
    assert!(survivors > 0, "the source must have survivors to relocate");

    // Drive one merge copy-out by hand: claim the source (what merge_evict
    // does before touching a candidate), take a destination, copy.
    assert!(
        cache.segments.claim_for_drain_for_test(src_id),
        "the source must be a claimable Sealed segment"
    );
    let dst_id = cache
        .segments
        .reserve_free()
        .expect("a free segment must be available as the copy destination");
    {
        let (mut src, mut dst) = cache
            .segments
            .segment_pair(src_id, dst_id)
            .expect("distinct valid segment ids");
        src.copy_into(&mut dst, &cache.hashtable)
            .expect("an uncontended copy_into must not hit RelinkFailure");
    }

    // Non-vacuity: the copy really did relocate every survivor.
    assert_eq!(
        cache.segments.header(dst_id).live_items(),
        survivors,
        "the copy must have relocated every survivor (otherwise this test \
         proves nothing about relocation)"
    );
    assert_eq!(
        cache.segments.header(src_id).live_items(),
        0,
        "the source must be emptied of live items"
    );

    assert_eq!(
        (
            cache.segments.header(src_id).dead_items(),
            cache.segments.header(src_id).dead_bytes()
        ),
        (dead_items, dead_bytes),
        "copy_into charged the source for items it RELOCATED — a moved item \
         is not a dead item, so the {survivors} survivors must not appear in \
         the source's dead total"
    );
    assert_eq!(
        cache.segments.header(dst_id).dead_items(),
        0,
        "the destination received live items only"
    );
}

/// The same property at the other relocation site: an S3-FIFO promotion.
///
/// Neutering check: drop the `src.decr_dead_item(...)` line from
/// `Segments::s3fifo_promote_from` and the source's dead total jumps from the
/// two deleted items to every item it ever held.
#[test]
fn s3fifo_promotion_does_not_charge_the_source_with_dead_space() {
    let cache = cache_with(
        Policy::S3Fifo {
            admission_ratio: 0.5,
        },
        TOTAL_SEGMENTS,
    );
    let ttl = Duration::from_secs(3600);

    let src_id = fill_two_segments(&cache, ttl);

    // Promotion only moves items with a non-zero frequency counter, so heat
    // the whole fill first.
    for _ in 0..3 {
        for i in 0..2 * ITEMS_PER_SEGMENT {
            assert!(
                cache.get(key_of(i).as_bytes()).is_some(),
                "heating lookup must hit — the fill must not have evicted"
            );
        }
    }

    let (dead_items, dead_bytes) = delete_until_dead(&cache, src_id, 2);
    assert_eq!(
        cache.segments.header(src_id).dead_bytes(),
        dead_bytes,
        "a delete must charge its item's space to the segment"
    );
    let survivors = cache.segments.header(src_id).live_items();
    assert!(survivors > 0, "the source must have survivors to promote");

    assert!(
        cache.segments.claim_for_drain_for_test(src_id),
        "the source must be a claimable Sealed segment"
    );
    let dst_id = cache
        .segments
        .reserve_free()
        .expect("a free segment must be available as the promotion destination");
    cache
        .segments
        .s3fifo_promote_from_for_test(src_id, dst_id, &cache.hashtable);

    assert_eq!(
        cache.segments.header(dst_id).live_items(),
        survivors,
        "the promotion must have moved every heated survivor (otherwise this \
         test proves nothing about relocation)"
    );

    assert_eq!(
        (
            cache.segments.header(src_id).dead_items(),
            cache.segments.header(src_id).dead_bytes()
        ),
        (dead_items, dead_bytes),
        "s3fifo_promote_from charged the source for items it PROMOTED — a \
         moved item is not a dead item"
    );
}

/// The reclaim half, at the per-segment level: recycling a segment zeroes its
/// dead charge, and does so IDEMPOTENTLY — `recycle` resets it and the later
/// `try_reserve` resets it again, so a second reset must find nothing left to
/// give back (otherwise the global gauge would be double-subtracted and run
/// negative).
#[test]
fn recycle_reclaims_dead_space_exactly_once() {
    let cache = cache_with(
        Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        },
        TOTAL_SEGMENTS,
    );
    let ttl = Duration::from_secs(3600);

    let src_id = fill_two_segments(&cache, ttl);
    let (dead_items, _) = delete_until_dead(&cache, src_id, 3);
    assert_eq!(cache.segments.header(src_id).dead_items(), dead_items);

    // Drain the segment out of the hashtable: its remaining live items die
    // (dead charge rises to the full complement), then it is recycled.
    assert!(cache.segments.claim_for_drain_for_test(src_id));
    let outcome = cache
        .segments
        .finalize_drained_for_test(src_id, &cache.hashtable);
    assert!(
        matches!(outcome, ClearOutcome::Freed),
        "an unpinned drained segment must be recycled, not condemned"
    );

    assert_eq!(
        cache.segments.header(src_id).dead_items(),
        0,
        "recycle must reclaim the segment's dead space"
    );
    assert_eq!(cache.segments.header(src_id).dead_bytes(), 0);

    // Second reset (the reserve-time one) must be a no-op, and it has to run
    // on THIS segment to prove anything: `reserve_free()` steals from the
    // front of the free queue and hands back whichever virgin segment is
    // there (observed: a different id, never recycled, never charged), so
    // asserting zeroes on that one is vacuous.
    //
    // `reset_write_stats` `swap(0)`s the per-segment counters and subtracts
    // from the global gauges only what it took, so the zero just asserted
    // above is precisely what makes this second pass subtract nothing; the
    // read after the reserve confirms it neither resurrected a charge nor
    // left one behind for the next tenant.
    assert_eq!(
        cache.segments.header(src_id).state(),
        State::Free,
        "the recycled segment must be Free and therefore reservable"
    );
    assert!(
        cache.segments.header(src_id).try_reserve(),
        "the recycled segment must be reservable again"
    );
    assert_eq!(
        (
            cache.segments.header(src_id).dead_items(),
            cache.segments.header(src_id).dead_bytes()
        ),
        (0, 0),
        "the reserve-time reset found dead space still charged to a segment \
         `recycle` had already reclaimed — the gauges are being subtracted \
         twice and will run negative"
    );
}
