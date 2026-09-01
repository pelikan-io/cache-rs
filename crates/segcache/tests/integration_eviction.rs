// Model-checking backends replace the lib's sync primitives, which panic
// outside their runner — compile this std-thread suite out rather than
// relying on the model jobs' name filters to skip it.
#![cfg(not(model_checking))]

use segcache::*;
use std::time::Duration;

// Segment size chosen so items fill segments the same way as integration_basic.rs.
const SEGMENT_SIZE: i32 = 264;

fn small_cache(segments: usize, policy: Policy) -> Segcache {
    Segcache::builder()
        .segment_size(SEGMENT_SIZE)
        .heap_size(segments * SEGMENT_SIZE as usize)
        .hash_power(16)
        .eviction(policy)
        .build()
        .expect("failed to create cache")
}

// ── Bug: can_evict() blocks segments with short TTL ───────────────────────────
//
// can_evict() used to require (create_at + ttl) >= (now + 20s), which is
// equivalent to "remaining TTL >= 20s". Any segment whose items have a TTL
// shorter than 20 s could never be explicitly evicted — can_evict() returned
// false for them unconditionally — causing NoFreeSegments when the cache was
// full of such items.

#[test]
fn random_evicts_short_ttl_segment_when_full() {
    let cache = small_cache(2, Policy::Random);
    let ttl = Duration::from_secs(10); // < former SEG_MATURE_TIME (20s)

    // Fill both segments (mirroring integration_basic sizes).
    let _ = cache.insert(
        b"a",
        b"What's in a name? A rose by any other name would smell as sweet.",
        None,
        ttl,
    );
    let _ = cache.insert(b"b", b"All that glitters is not gold.", None, ttl);
    let _ = cache.insert(
        b"c",
        b"Cry 'havoc' and let slip the dogs of war.",
        None,
        ttl,
    );
    // segment 1 is now full

    let _ = cache.insert(
        b"d",
        b"There are more things in heaven and earth, Horatio, than are dreamt of in your philosophy.",
        None,
        ttl,
    );
    let _ = cache.insert(
        b"e",
        b"Uneasy lies the head that wears the crown.",
        None,
        ttl,
    );
    let _ = cache.insert(b"f", b"Brevity is the soul of wit.", None, ttl);
    #[cfg(not(feature = "integrity"))]
    let _ = cache.insert(
        b"g",
        b"But, for my own part, it was Greek to me.",
        None,
        ttl,
    );
    #[cfg(feature = "integrity")]
    let _ = cache.insert(b"g", b"Et tu, Brute?", None, ttl);
    // segment 2 is now full

    // This insert needs a free segment. It must evict segment 1 (which holds
    // short-TTL items). Before the fix can_evict() always returned false for
    // these segments, so this returned Err(NoFreeSegments).
    let result = cache.insert(
        b"h",
        b"There is nothing either good or bad, but thinking makes it so.",
        None,
        ttl,
    );

    assert!(
        result.is_ok(),
        "inserting into a full cache of short-TTL items should succeed via eviction"
    );
    assert!(
        cache.get(b"h").is_some(),
        "newly inserted item must be readable"
    );
}

// ── Bug: compare_fifo sorts by NEWEST first (LIFO) instead of OLDEST (FIFO) ──
//
// compare_fifo called lhs_age.cmp(&rhs_age).reverse(). sort_by is ascending,
// so .reverse() placed the segment with the LARGEST timestamp (most recently
// created/merged) at index 0, which is the slot evicted first. This is LIFO.
// Removing .reverse() makes the oldest segment sort first — correct FIFO.
//
// clocksource::coarse::Instant has 1-second resolution (stored as whole
// seconds), so we need at least a 1-second sleep to produce timestamps that
// the comparator can distinguish.

#[test]
fn fifo_evicts_oldest_segment_first() {
    // 3 segments: seg1 (old items) -> seg2 (new items) -> seg3 (tail/current
    // write target). Eviction must choose between seg1 and seg2; seg3 is
    // never eligible because it has no next_seg. Correct FIFO picks seg1
    // (the oldest).
    //
    // LAYOUT-PROOF SIZING: uniform items and a segment size computed from
    // the real item footprint, so "three items fill a segment exactly"
    // holds in every feature combination (the item header is 6 bytes by
    // default and 12 under `integrity`, which also prefixes each segment
    // with 8 magic bytes — a hand-tuned byte count silently repacks when
    // the layout changes, which is exactly how this test broke when the
    // CRC stopped being always-on).
    const ITEMS_PER_SEG: usize = 3;
    const KLEN: usize = 5;
    const VLEN: usize = 40;
    let value = [b'x'; VLEN];
    let item_size = keyvalue::item_size(KLEN, &keyvalue::Value::Bytes(&value), 0);
    let seg_base = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (seg_base + ITEMS_PER_SEG * item_size) as i32;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(3 * segment_size as usize)
        .hash_power(16)
        .eviction(Policy::Fifo)
        .build()
        .expect("failed to create cache");
    let ttl = Duration::ZERO;

    let insert = |key: &[u8]| {
        assert_eq!(key.len(), KLEN, "uniform sizing requires {KLEN}-byte keys");
        cache
            .insert(key, &value[..], None, ttl)
            .expect("insert must succeed");
    };

    // Fill segment 1 with "old" items.
    insert(b"old_a");
    insert(b"old_b");
    insert(b"old_c");
    // segment 1 is exactly full and seals on the next insert.

    // Sleep long enough for clocksource::coarse (1-second resolution) to
    // tick, so the FIFO comparator can distinguish the segments' ages.
    std::thread::sleep(Duration::from_millis(1100));

    // Fill segment 2 with "new" items.
    insert(b"new_d");
    insert(b"new_e");
    insert(b"new_f");

    // Fill segment 3 so that the next insert must evict. (Without this,
    // the trigger would fit in the still-empty tail and no eviction would
    // occur.)
    insert(b"thr_a");
    insert(b"thr_b");
    insert(b"thr_c");

    // Trigger eviction: the next insert needs a free segment.
    insert(b"trigX");

    // Correct FIFO: the oldest segment (seg1, "old_*") is evicted.
    for key in [b"old_a", b"old_b", b"old_c"] {
        assert!(
            cache.get(key).is_none(),
            "FIFO must evict the oldest segment: {} should be gone",
            String::from_utf8_lossy(key)
        );
    }

    // The newer segment (seg2, "new_*") must still be present.
    for key in [b"new_d", b"new_e", b"new_f"] {
        assert!(
            cache.get(key).is_some(),
            "FIFO must not evict the newer segment: {} should be present",
            String::from_utf8_lossy(key)
        );
    }
}

// Regression: insert() into a FULL Merge pool must complete (not livelock).
//
// Bug: Segment::prune()'s adaptive cutoff could collapse to 0 (t == -1 when
// no bytes retained yet at the first checkpoint), permanently disabling the
// cutoff>=0.0001 drop-gate so a merge candidate was retained whole. With the
// Merge spare (spare_capacity 1) that fed only the spare, never the general
// free queue, and reserve_and_define's retry loop spun forever on an evict()
// that returned Ok without freeing a usable segment. This drove insert() into
// an infinite loop on a full Merge cache. Fixed by flooring the prune cutoff
// multiplier and bounding the retry loop to eviction that actually raises the
// free queue.
#[test]
fn merge_full_pool_insert_makes_progress_no_livelock() {
    let cache = small_cache(
        8,
        Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        },
    );
    let ttl = Duration::from_secs(3600);

    // Fill well past capacity with distinct keys. Each insert must return
    // promptly (Ok after eviction, or a bounded NoFreeSegments) — never hang.
    let mut ok = 0usize;
    for i in 0..3000usize {
        let k = format!("k{i:06}");
        let v = format!("v{i:06}");
        if cache.insert(k.as_bytes(), v.as_bytes(), None, ttl).is_ok() {
            ok += 1;
        }
    }

    // The vast majority of inserts must succeed — eviction reclaims space, so
    // the cache keeps accepting writes rather than wedging.
    assert!(
        ok >= 2900,
        "expected nearly all inserts to succeed after eviction, got {ok}/3000"
    );

    // And the cache is still usable: the most recent key resolves.
    cache
        .insert(b"final", b"value", None, ttl)
        .expect("insert into a churned Merge cache must succeed");
    assert_eq!(cache.get(b"final").unwrap().value(), b"value");
}
