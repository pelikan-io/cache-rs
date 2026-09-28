//! S3-FIFO's main queue is FIFO with reinsertion: an item found with a
//! nonzero frequency when its segment is evicted gets a second chance -- it
//! is copied forward -- and its frequency is decremented, so an item that is
//! not read again falls to zero and is dropped on a later pass. Without the
//! decrement every item keeps whatever frequency it had, a second chance
//! never ends, and an eviction pass copies a segment into another instead
//! of freeing it.

use std::time::Duration;

use crate::hashtable::{unpack_location, Hashtable};
use crate::segments::SegmentPool;
use crate::{Location, Policy, Segcache};
use core::num::NonZeroU32;

const ITEMS_PER_SEGMENT: usize = 8;
const KEY_LEN: usize = 7;
const VALUE: &[u8] = b"payload";

fn cache() -> Segcache {
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
    let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic + item_size * ITEMS_PER_SEGMENT) as i32;
    Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * 16)
        .hash_power(16)
        .eviction(Policy::S3Fifo {
            admission_ratio: 0.25,
        })
        .build()
        .expect("failed to create cache")
}

/// Location and frequency, read without bumping the frequency.
fn state(cache: &Segcache, key: &str) -> Option<(NonZeroU32, u8)> {
    let verifier = cache.segments.verifier();
    let loc: Location = cache
        .hashtable
        .lookup_no_freq_update(key.as_bytes(), &verifier)
        .found()?
        .location;
    let freq = cache.hashtable.get_item_frequency(key.as_bytes(), loc)?;
    Some((NonZeroU32::new(unpack_location(loc).0)?, freq))
}

fn evict(cache: &Segcache) -> bool {
    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .is_ok()
}

#[test]
fn a_second_chance_costs_a_frequency_step() {
    let cache = cache();
    let ttl = Duration::from_secs(3600);
    // Two sealed admission segments and a Live tail. Never read: each item
    // sits at the frequency an insert gives it.
    let keys: Vec<String> = (0..=2 * ITEMS_PER_SEGMENT)
        .map(|i| format!("k{i:06}"))
        .collect();
    for k in &keys {
        cache.insert(k.as_bytes(), VALUE, None, ttl).expect("fill");
    }
    let key = &keys[0];
    let (_, inserted) = state(&cache, key).expect("inserted");

    // Promote both admission segments into the main pool.
    assert!(evict(&cache) && evict(&cache), "admission evictions");
    let (promoted_to, at_promotion) = state(&cache, key).expect("promoted, not dropped");
    assert_eq!(cache.segments.header(promoted_to).pool(), SegmentPool::Main);
    assert_eq!(
        at_promotion, inserted,
        "promotion itself leaves frequency alone"
    );
    assert!(
        at_promotion > 0,
        "fixture: the item must qualify for a second chance"
    );

    // Evict until the main-pool segment holding it has been through a pass.
    let mut after = None;
    for _ in 0..8 {
        if !evict(&cache) {
            break;
        }
        match state(&cache, key) {
            Some((seg, freq)) if seg != promoted_to => {
                after = Some(freq);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    let after = after.expect("the item was given a second chance: copied to another main segment");
    assert_eq!(
        after,
        at_promotion - 1,
        "a second chance must cost a frequency step, or it never ends"
    );
}

/// The consequence: an item never read again is eventually dropped.
#[test]
fn an_item_that_is_never_read_does_not_survive_main_eviction_forever() {
    let cache = cache();
    let ttl = Duration::from_secs(3600);
    let keys: Vec<String> = (0..=2 * ITEMS_PER_SEGMENT)
        .map(|i| format!("k{i:06}"))
        .collect();
    for k in &keys {
        cache.insert(k.as_bytes(), VALUE, None, ttl).expect("fill");
    }
    // Promote, then keep evicting from the main pool. Each unread item's
    // frequency starts at the insert value, so it may be copied forward that
    // many times and no more.
    //
    // A second per pass, as real time does. With a frozen clock every main
    // segment has the same age, the tie goes to chain order, and the
    // second-chance target sits in its source's place -- so the pool keeps
    // re-selecting the same position and never reaches the rest. That
    // starvation is real within a burst of same-second evictions and is a
    // separate fault from the one pinned here.
    for s in 0..40 {
        crate::clock::set_virtual_now(1_000_000 + s);
        if !evict(&cache) {
            break;
        }
    }
    crate::clock::clear_virtual_now();
    let survivors = keys[..2 * ITEMS_PER_SEGMENT]
        .iter()
        .filter(|k| state(&cache, k).is_some())
        .count();
    assert_eq!(
        survivors, 0,
        "{survivors} never-read items survived 40 eviction passes"
    );
}
