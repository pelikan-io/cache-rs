use crate::*;
use std::time::Duration;

#[test]
fn init() {
    let cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");
    assert_eq!(cache.items(), 0);
}

#[test]
fn insert_and_get() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"hello", b"world", None, Duration::ZERO)
        .expect("insert failed");
    assert_eq!(cache.items(), 1);

    let item = cache.get(b"hello").expect("item not found");
    assert_eq!(item.value(), b"world");
}

#[test]
fn insert_replaces_existing() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"key", b"value1", None, Duration::ZERO)
        .expect("insert failed");
    cache
        .insert(b"key", b"value2", None, Duration::ZERO)
        .expect("insert failed");

    assert_eq!(cache.items(), 1);
    let item = cache.get(b"key").expect("item not found");
    assert_eq!(item.value(), b"value2");
}

#[test]
fn delete() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    assert!(!cache.delete(b"key"));

    cache
        .insert(b"key", b"value", None, Duration::ZERO)
        .expect("insert failed");
    assert_eq!(cache.items(), 1);

    assert!(cache.delete(b"key"));
    assert_eq!(cache.items(), 0);
    assert!(cache.get(b"key").is_none());
}

#[test]
fn eviction_under_pressure() {
    // Small cache: 256 bytes total, 10% small = 25 bytes
    let mut cache = S3Fifo::builder()
        .heap_size(256)
        .hash_power(8)
        .small_queue_ratio(0.10)
        .build()
        .expect("failed to create cache");

    // Fill the cache with items until eviction occurs
    for i in 0u64..100 {
        let key = format!("key{i:04}");
        let val = format!("val{i:04}");
        let _ = cache.insert(key.as_bytes(), val.as_bytes(), None, Duration::ZERO);
    }

    // We should have some items but not all 100
    assert!(cache.items() > 0);
    assert!(cache.items() < 100);
}

#[test]
fn frequency_promotes_to_main() {
    // Items accessed in small queue should be promoted to main during eviction
    let mut cache = S3Fifo::builder()
        .heap_size(512)
        .hash_power(8)
        .small_queue_ratio(0.10)
        .build()
        .expect("failed to create cache");

    // Insert a "hot" item and access it to raise frequency
    cache
        .insert(b"hot", b"item", None, Duration::ZERO)
        .expect("insert failed");
    let _ = cache.get(b"hot"); // freq = 1
    let _ = cache.get(b"hot"); // freq = 2

    // Fill cache to trigger eviction — hot item should survive via promotion
    for i in 0u64..50 {
        let key = format!("fill{i:04}");
        let _ = cache.insert(key.as_bytes(), b"data", None, Duration::ZERO);
    }

    // The hot item should still be present (promoted to main)
    assert!(cache.get(b"hot").is_some());
}

#[test]
fn clear() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    for i in 0..10 {
        let key = format!("key{i}");
        let _ = cache.insert(key.as_bytes(), b"value", None, Duration::ZERO);
    }
    assert_eq!(cache.items(), 10);

    let cleared = cache.clear();
    assert_eq!(cleared, 10);
    assert_eq!(cache.items(), 0);
}

#[test]
fn get_no_freq_incr() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"key", b"value", None, Duration::ZERO)
        .expect("insert failed");

    // get_no_freq_incr should return the item but not bump frequency
    let item = cache.get_no_freq_incr(b"key").expect("item not found");
    assert_eq!(item.value(), b"value");
}

#[test]
fn cas_operation() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    // CAS on missing item fails
    assert_eq!(
        cache.cas(b"key", b"val", None, Duration::ZERO, 0),
        Err(S3FifoError::NotFound)
    );

    cache
        .insert(b"key", b"val1", None, Duration::ZERO)
        .expect("insert failed");

    // CAS with wrong value fails
    assert_eq!(
        cache.cas(b"key", b"val2", None, Duration::ZERO, 0),
        Err(S3FifoError::Exists)
    );

    // CAS with correct value succeeds
    let item = cache.get(b"key").expect("not found");
    let cas_val = item.cas();
    assert!(cache
        .cas(b"key", b"val2", None, Duration::ZERO, cas_val)
        .is_ok());

    let item = cache.get(b"key").expect("not found");
    assert_eq!(item.value(), b"val2");
}

#[test]
fn numeric_operations() {
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"counter", 0_u64, None, Duration::ZERO)
        .expect("insert failed");

    let item = cache.wrapping_add(b"counter", 5).expect("add failed");
    assert_eq!(item.value(), 5_u64);

    let item = cache.saturating_sub(b"counter", 3).expect("sub failed");
    assert_eq!(item.value(), 2_u64);

    let item = cache.saturating_sub(b"counter", 10).expect("sub failed");
    assert_eq!(item.value(), 0_u64);
}

#[test]
fn ghost_readmission() {
    // Test that items evicted from small and then reinserted go to main
    let mut cache = S3Fifo::builder()
        .heap_size(256)
        .hash_power(8)
        .small_queue_ratio(0.10)
        .build()
        .expect("failed to create cache");

    // Insert and evict an item by filling the cache
    let _ = cache.insert(b"victim", b"data", None, Duration::ZERO);

    for i in 0u64..50 {
        let key = format!("fill{i:04}");
        let _ = cache.insert(key.as_bytes(), b"padding!", None, Duration::ZERO);
    }

    // victim was likely evicted from small (freq==0) and its hash added to ghost
    // Reinserting should place it in main via ghost hit
    let _ = cache.insert(b"victim", b"data", None, Duration::ZERO);

    // Access it to confirm it exists
    assert!(cache.get(b"victim").is_some());
}
