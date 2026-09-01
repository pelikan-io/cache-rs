// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rand::Rng;
use rand::SeedableRng;
use segcache::*;

use std::time::Duration;

pub const MB: usize = 1024 * 1024;

// A very fast PRNG which is appropriate for testing
pub fn rng() -> impl Rng {
    rand_xoshiro::Xoshiro256PlusPlus::seed_from_u64(0)
}

/// The number of keys the `get_hit` benchmarks populate and then cycle.
///
/// A quarter of the 2^16 slots `hash_power(16)` provides. Buckets hold 8
/// slots and do not chain, so an unlucky bucket that fills fails its insert
/// outright; measured, that starts happening around 24k keys, and a 25% load
/// factor leaves comfortable headroom. The resulting working set — 16384
/// items, ~4MB at the 255b key size — is far inside the 64MB heap, so
/// nothing is evicted either and every subsequent get is a hit.
/// `get_hit_benchmark` asserts that rather than assuming it.
const HIT_KEY_COUNT: usize = 16_384;

/// The MISS path: an empty cache, so every `get` returns `None` after a
/// single failed hashtable lookup.
///
/// This group never inserts anything. It cannot detect a regression on the
/// hit path — the pin, the key verification and the revalidation retry are
/// all downstream of the lookup that fails here, and none of them run. Pair
/// every read-path measurement with `get_hit` below; the two groups share a
/// key/value size matrix and a cache configuration so their numbers are
/// directly comparable.
fn get_miss_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_miss");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    for key_size in [1, 255].iter() {
        let (keys, _values) = key_values(*key_size, 1_000_000, 0, 0);

        // launch the server
        let cache = Segcache::builder()
            .hash_power(16)
            .heap_size(64 * MB)
            .segment_size(MB as i32)
            .build()
            .expect("failed to create cache");

        let mut key = 0;

        group.bench_function(format!("{key_size}b/0b"), |b| {
            b.iter(|| {
                cache.get(&keys[key]);
                key += 1;
                if key >= keys.len() {
                    key = 0;
                }
            })
        });
    }
}

/// The HIT path: a populated cache, so every `get` resolves the lookup,
/// pins the segment, verifies the key and hands the item back.
///
/// Same key/value size matrix, same builder configuration and same measured
/// loop as `get_miss`, so the two groups differ only in whether the key is
/// resident.
///
/// Note that at the 1b key size there are only 256 distinct keys, so the
/// resident set collapses to those and the working set is L1-sized whatever
/// `HIT_KEY_COUNT` says. That is inherent to the size matrix and applies to
/// `get_miss` too; the 255b case is the one that exercises memory.
fn get_hit_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("get_hit");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    for key_size in [1, 255].iter() {
        let (keys, _values) = key_values(*key_size, HIT_KEY_COUNT, 0, 0);

        // launch the server
        let cache = Segcache::builder()
            .hash_power(16)
            .heap_size(64 * MB)
            .segment_size(MB as i32)
            .build()
            .expect("failed to create cache");

        for key in &keys {
            cache
                .insert(&key[..], &[][..], None, ttl)
                .expect("failed to populate the cache: HIT_KEY_COUNT too large?");
        }

        // A benchmark that quietly stopped hitting would still produce
        // plausible numbers — it would just be measuring `get_miss` twice.
        // So prove the population held, over the exact key array the
        // measured loop cycles, before measuring anything.
        assert_hits(&cache, &keys, "after populating");

        let mut key = 0;

        group.bench_function(format!("{key_size}b/0b"), |b| {
            b.iter(|| {
                cache.get(&keys[key]);
                key += 1;
                if key >= keys.len() {
                    key = 0;
                }
            })
        });

        // ...and that it still held once the measurement was done, so
        // eviction or expiry part-way through cannot go unnoticed either.
        assert_hits(&cache, &keys, "after measuring");
    }
}

/// Panics unless every key in `keys` resolves to an item carrying that key.
///
/// Checking the returned key, not just `is_some()`, means a lookup that
/// resolved to the wrong item counts as the failure it is.
fn assert_hits(cache: &Segcache, keys: &[Vec<u8>], when: &str) {
    let hits = keys
        .iter()
        .filter(|key| {
            cache
                .get(&key[..])
                .is_some_and(|item| item.key() == &key[..])
        })
        .count();

    assert_eq!(
        hits,
        keys.len(),
        "get_hit must measure hits: only {hits}/{} keys were resident {when}",
        keys.len(),
    );
}

fn key_values(
    key_size: usize,
    key_count: usize,
    value_size: usize,
    value_count: usize,
) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut rng = rng();

    let mut keys = Vec::new();
    for _ in 0..key_count {
        let mut key = vec![0; key_size];
        rng.fill_bytes(&mut key);
        keys.push(key);
    }

    let mut values = Vec::new();
    for _ in 0..value_count {
        let mut value = vec![0; value_size];
        rng.fill_bytes(&mut value);
        values.push(value);
    }

    (keys, values)
}

fn set_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("set");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    for key_size in [1, 255].iter() {
        for value_size in [1, 64, 1024, 16384].iter() {
            let (keys, values) = key_values(*key_size, 1_000_000, *value_size, 10_000);

            // launch the server
            let cache = Segcache::builder()
                .hash_power(16)
                .heap_size(64 * MB)
                .segment_size(MB as i32)
                .build()
                .expect("failed to create cache");

            let mut key = 0;
            let mut value = 0;

            group.bench_function(format!("{key_size}b/{value_size}b"), |b| {
                b.iter(|| {
                    let _ = cache.insert(&keys[key], &values[value], None, ttl);
                    key += 1;
                    if key >= keys.len() {
                        key = 0;
                    }
                    value += 1;
                    if value >= values.len() {
                        value = 0;
                    }
                })
            });
        }
    }
}

fn set_fresh_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("set_fresh");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    // Monotonically unique keys: every op is a genuine FRESH-key insert.
    // The `set` bench cycles a fixed 1M-key set and is mostly overwrites
    // after warmup, diluting exactly the fresh-key claim path this bench
    // isolates. hash_power 20 (1M slots) comfortably exceeds what the
    // 64MB heap holds live, so inserts exercise the claim path rather
    // than the table-full error path; eviction churn is part of the
    // steady-state miss-fill cost being measured.
    let cache = Segcache::builder()
        .hash_power(20)
        .heap_size(64 * MB)
        .segment_size(MB as i32)
        .build()
        .expect("failed to create cache");

    let value = [0u8; 64];
    let mut counter: u64 = 0;

    group.bench_function("8b/64b", |b| {
        b.iter(|| {
            let key = counter.to_be_bytes();
            counter += 1;
            let _ = cache.insert(&key, &value[..], None, ttl);
        })
    });
}

fn incr_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("incr");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    // a single hot counter: the worst case for the republish design
    // (every increment writes a new item; sustained churn exercises
    // steady-state eviction, which is part of the honest cost)
    let cache = Segcache::builder()
        .hash_power(16)
        .heap_size(64 * MB)
        .segment_size(MB as i32)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"counter", 0, None, ttl)
        .expect("failed to insert");

    group.bench_function("hot_counter", |b| {
        b.iter(|| {
            let _ = cache.wrapping_add(b"counter", 1);
        })
    });
}

fn cas_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("cas");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    // the realistic gets -> cas round trip on a single key
    let cache = Segcache::builder()
        .hash_power(16)
        .heap_size(64 * MB)
        .segment_size(MB as i32)
        .build()
        .expect("failed to create cache");

    cache
        .insert(b"key", &[0xABu8; 64][..], None, ttl)
        .expect("failed to insert");
    let value = [0xCDu8; 64];

    group.bench_function("gets_cas/64b", |b| {
        b.iter(|| {
            let token = cache.get(b"key").unwrap().cas();
            let _ = cache.cas(b"key", &value[..], None, ttl, token);
        })
    });
}

criterion_group!(
    benches,
    get_miss_benchmark,
    get_hit_benchmark,
    set_benchmark,
    set_fresh_benchmark,
    incr_benchmark,
    cas_benchmark,
);
criterion_main!(benches);
