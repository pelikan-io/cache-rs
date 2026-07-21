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

fn get_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");
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
            let mut cache = Segcache::builder()
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

fn incr_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("incr");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    // a single hot counter: the worst case for the republish design
    // (every increment writes a new item; sustained churn exercises
    // steady-state eviction, which is part of the honest cost)
    let mut cache = Segcache::builder()
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
    let mut cache = Segcache::builder()
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
    get_benchmark,
    set_benchmark,
    incr_benchmark,
    cas_benchmark,
);
criterion_main!(benches);
