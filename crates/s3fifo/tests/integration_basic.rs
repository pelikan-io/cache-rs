use s3fifo::*;

use std::time::Duration;

#[test]
fn integration_basic() {
    let ttl = Duration::ZERO;
    let mut cache = S3Fifo::builder()
        .heap_size(4096 * 64)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    // Insert and retrieve
    let _ = cache.insert(
        b"a",
        b"What's in a name? A rose by any other name would smell as sweet.",
        None,
        ttl,
    );
    assert_eq!(cache.get(b"a").map(|v| v.value().len()), Some(64));

    let _ = cache.insert(b"b", b"All that glitters is not gold.", None, ttl);
    assert_eq!(cache.get(b"a").map(|v| v.value().len()), Some(64));
    assert_eq!(cache.get(b"b").map(|v| v.value().len()), Some(30));

    let _ = cache.insert(
        b"c",
        b"Cry 'havoc' and let slip the dogs of war.",
        None,
        ttl,
    );
    assert_eq!(cache.get(b"a").map(|v| v.value().len()), Some(64));
    assert_eq!(cache.get(b"b").map(|v| v.value().len()), Some(30));
    assert_eq!(cache.get(b"c").map(|v| v.value().len()), Some(41));

    // Delete
    assert!(cache.delete(b"b"));
    assert!(cache.get(b"b").is_none());
    assert!(cache.get(b"a").is_some());
    assert!(cache.get(b"c").is_some());

    // Overwrite
    let _ = cache.insert(b"a", b"updated!", None, ttl);
    let item = cache.get(b"a").expect("not found");
    assert_eq!(item.value(), b"updated!");

    // Clear
    let count = cache.clear();
    assert!(count >= 2);
    assert_eq!(cache.items(), 0);
    assert!(cache.get(b"a").is_none());
    assert!(cache.get(b"c").is_none());
}

#[test]
fn integration_fill_and_evict() {
    let ttl = Duration::ZERO;
    let mut cache = S3Fifo::builder()
        .heap_size(1024)
        .hash_power(10)
        .small_queue_ratio(0.10)
        .build()
        .expect("failed to create cache");

    // Fill cache well past capacity
    for i in 0u64..200 {
        let key = format!("k{i:06}");
        let val = format!("v{i:06}");
        let _ = cache.insert(key.as_bytes(), val.as_bytes(), None, ttl);
    }

    // Cache should have items but be bounded by heap_size
    assert!(cache.items() > 0);

    // We should be able to insert and retrieve recent items
    let key = "k000199";
    let _ = cache.insert(key.as_bytes(), b"final", None, ttl);
    assert!(cache.get(key.as_bytes()).is_some());
}

#[test]
fn integration_numeric() {
    let ttl = Duration::ZERO;
    let mut cache = S3Fifo::builder()
        .heap_size(4096)
        .hash_power(10)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"counter", 100_u64, None, ttl)
        .expect("insert failed");

    let item = cache.wrapping_add(b"counter", 50).expect("add failed");
    assert_eq!(item.value(), 150_u64);

    let item = cache
        .saturating_sub(b"counter", 200)
        .expect("sub failed");
    assert_eq!(item.value(), 0_u64);

    // Non-numeric wrapping_add should fail
    cache
        .insert(b"text", b"hello", None, ttl)
        .expect("insert failed");
    assert!(cache.wrapping_add(b"text", 1).is_err());
}
