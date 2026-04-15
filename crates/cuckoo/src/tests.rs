// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

use super::*;

use std::time::Duration;

#[test]
fn init() {
    let cache = CuckooCache::builder()
        .nitem(1024)
        .item_size(64)
        .build();
    assert_eq!(cache.items(), 0);
}

#[test]
fn get_miss() {
    let mut cache = CuckooCache::builder().build();
    assert!(cache.get(b"coffee").is_none());
}

#[test]
fn get_hit() {
    let mut cache = CuckooCache::builder().build();
    assert!(cache.get(b"coffee").is_none());
    cache
        .insert(b"coffee", b"strong", None, Duration::ZERO)
        .unwrap();
    let item = cache.get(b"coffee").unwrap();
    assert_eq!(item.value(), b"strong");
}

#[test]
fn overwrite() {
    let mut cache = CuckooCache::builder().build();

    cache
        .insert(b"drink", b"coffee", None, Duration::ZERO)
        .unwrap();
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink").unwrap();
    assert_eq!(item.value(), b"coffee");

    cache
        .insert(b"drink", b"espresso", None, Duration::ZERO)
        .unwrap();
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink").unwrap();
    assert_eq!(item.value(), b"espresso");

    cache
        .insert(b"drink", b"whisky", None, Duration::ZERO)
        .unwrap();
    assert_eq!(cache.items(), 1);
    let item = cache.get(b"drink").unwrap();
    assert_eq!(item.value(), b"whisky");
}

#[test]
fn delete() {
    let mut cache = CuckooCache::builder().build();

    assert!(!cache.delete(b"coffee"));

    cache
        .insert(b"coffee", b"strong", None, Duration::ZERO)
        .unwrap();
    assert!(cache.get(b"coffee").is_some());
    assert_eq!(cache.items(), 1);

    assert!(cache.delete(b"coffee"));
    assert!(cache.get(b"coffee").is_none());
    assert_eq!(cache.items(), 0);
}

#[test]
fn delete_miss() {
    let mut cache = CuckooCache::builder().build();
    assert!(!cache.delete(b"nonexistent"));
}

#[test]
fn item_oversized() {
    let mut cache = CuckooCache::builder().item_size(16).build();

    // 4 (expire) + 5 (hdr) + 9 (key) + 11 (val) = 29 > 16
    let result = cache.insert(b"large_key", b"large_value", None, Duration::ZERO);
    assert!(matches!(
        result,
        Err(CuckooCacheError::ItemOversized { .. })
    ));
}

#[test]
fn fill_and_evict() {
    let mut cache = CuckooCache::builder().nitem(16).item_size(64).build();

    // Insert more items than the table can hold
    for i in 0..32u32 {
        let key = format!("k{i:04}");
        let val = format!("v{i:04}");
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, Duration::ZERO)
            .unwrap();
    }

    // Should have at most nitem items
    assert!(cache.items() <= 16);
    // Should have some items (eviction doesn't empty the cache)
    assert!(cache.items() > 0);
}

#[test]
fn displacement() {
    // Use a table large enough that displacement can work but small enough
    // to trigger it
    let mut cache = CuckooCache::builder()
        .nitem(32)
        .item_size(64)
        .max_displace(2)
        .build();

    // Insert enough items to require displacement
    let mut inserted = 0;
    for i in 0..28u32 {
        let key = format!("key{i:04}");
        let val = format!("val{i:04}");
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, Duration::ZERO)
            .unwrap();
        inserted += 1;
    }

    // Verify items are retrievable
    let mut found = 0;
    for i in 0..inserted {
        let key = format!("key{i:04}");
        if cache.get(key.as_bytes()).is_some() {
            found += 1;
        }
    }
    // Most items should be found (some may have been evicted)
    assert!(
        found > inserted / 2,
        "only found {found} of {inserted} items"
    );
}

#[test]
fn numeric_value() {
    let mut cache = CuckooCache::builder().build();
    cache.insert(b"counter", 0u64, None, Duration::ZERO).unwrap();

    let item = cache.get(b"counter").unwrap();
    assert_eq!(item.value(), 0u64);
}

#[test]
fn wrapping_add() {
    let mut cache = CuckooCache::builder().build();
    cache
        .insert(b"counter", 10u64, None, Duration::ZERO)
        .unwrap();

    let item = cache.wrapping_add(b"counter", 5).unwrap();
    assert_eq!(item.value(), 15u64);

    let item = cache.get(b"counter").unwrap();
    assert_eq!(item.value(), 15u64);
}

#[test]
fn wrapping_add_overflow() {
    let mut cache = CuckooCache::builder().build();
    cache
        .insert(b"counter", u64::MAX, None, Duration::ZERO)
        .unwrap();

    let item = cache.wrapping_add(b"counter", 1).unwrap();
    assert_eq!(item.value(), 0u64);
}

#[test]
fn saturating_sub() {
    let mut cache = CuckooCache::builder().build();
    cache
        .insert(b"counter", 10u64, None, Duration::ZERO)
        .unwrap();

    let item = cache.saturating_sub(b"counter", 3).unwrap();
    assert_eq!(item.value(), 7u64);

    let item = cache.saturating_sub(b"counter", 100).unwrap();
    assert_eq!(item.value(), 0u64);
}

#[test]
fn wrapping_add_not_numeric() {
    let mut cache = CuckooCache::builder().build();
    cache
        .insert(b"str", b"hello", None, Duration::ZERO)
        .unwrap();

    assert!(matches!(
        cache.wrapping_add(b"str", 1),
        Err(CuckooCacheError::NotNumeric)
    ));
}

#[test]
fn wrapping_add_not_found() {
    let mut cache = CuckooCache::builder().build();
    assert!(matches!(
        cache.wrapping_add(b"missing", 1),
        Err(CuckooCacheError::NotFound)
    ));
}

#[test]
fn many_items() {
    let mut cache = CuckooCache::builder()
        .nitem(4096)
        .item_size(64)
        .build();

    for i in 0..2048u32 {
        let key = format!("key{i:06}");
        let val = format!("v{i:06}");
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, Duration::ZERO)
            .unwrap();
    }

    // Most should be retrievable (some may be evicted due to hash collisions)
    let mut found = 0;
    for i in 0..2048u32 {
        let key = format!("key{i:06}");
        if cache.get(key.as_bytes()).is_some() {
            found += 1;
        }
    }
    assert!(found > 1500, "only found {found} of 2048 items");
}

#[test]
fn clear() {
    let mut cache = CuckooCache::builder().build();

    for i in 0..10u32 {
        let key = format!("key{i}");
        cache
            .insert(key.as_bytes(), b"val", None, Duration::ZERO)
            .unwrap();
    }
    assert!(cache.items() > 0);

    cache.clear();
    assert_eq!(cache.items(), 0);
}

#[test]
fn expire_policy() {
    let mut cache = CuckooCache::builder()
        .nitem(16)
        .item_size(64)
        .policy(Policy::Expire)
        .build();

    for i in 0..32u32 {
        let key = format!("k{i:04}");
        let val = format!("v{i:04}");
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, Duration::ZERO)
            .unwrap();
    }

    assert!(cache.items() <= 16);
    assert!(cache.items() > 0);
}

#[test]
fn optional_data() {
    let mut cache = CuckooCache::builder().build();

    cache
        .insert(b"key", b"val", Some(b"opt"), Duration::ZERO)
        .unwrap();
    let item = cache.get(b"key").unwrap();
    assert_eq!(item.value(), b"val");
    assert_eq!(item.optional(), Some(b"opt".as_slice()));
}

#[test]
fn no_optional_data() {
    let mut cache = CuckooCache::builder().build();

    cache
        .insert(b"key", b"val", None, Duration::ZERO)
        .unwrap();
    let item = cache.get(b"key").unwrap();
    assert_eq!(item.optional(), None);
}

#[test]
fn multiple_distinct_keys() {
    let mut cache = CuckooCache::builder().build();

    cache
        .insert(b"a", b"alpha", None, Duration::ZERO)
        .unwrap();
    cache
        .insert(b"b", b"bravo", None, Duration::ZERO)
        .unwrap();
    cache
        .insert(b"c", b"charlie", None, Duration::ZERO)
        .unwrap();

    assert_eq!(cache.items(), 3);
    assert_eq!(cache.get(b"a").unwrap().value(), b"alpha");
    assert_eq!(cache.get(b"b").unwrap().value(), b"bravo");
    assert_eq!(cache.get(b"c").unwrap().value(), b"charlie");
}

#[test]
fn delete_then_reinsert() {
    let mut cache = CuckooCache::builder().build();

    cache
        .insert(b"key", b"first", None, Duration::ZERO)
        .unwrap();
    assert!(cache.delete(b"key"));
    assert!(cache.get(b"key").is_none());

    cache
        .insert(b"key", b"second", None, Duration::ZERO)
        .unwrap();
    let item = cache.get(b"key").unwrap();
    assert_eq!(item.value(), b"second");
}
