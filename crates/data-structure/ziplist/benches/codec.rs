// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Criterion benchmarks for the `ziplist` codec.
//!
//! Two groups:
//! - `entry_codec`: raw `encode_into`/`decode` throughput for representative
//!   entry shapes (each `Uint` tag tier, and a few string sizes).
//! - `ops`: per-op cost for `hget`/`hset`/`push_back`/`zadd` at
//!   `nentry` in `{8, 64, 512, 4096}` on a 64KB buffer -- the seam sweep
//!   from the spec's verification item 8 (P33), read from the codec side.
//!   `nentry` is the block header's raw entry count: for `hash`/`zset`,
//!   whose bodies are `(field/member, value/score)` pairs, that's
//!   `nentry / 2` logical fields/members.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use ziplist::{EntryVal, HashMut, ListMut, ZsetMut};

const KB: usize = 1024;
const BUF_64K: usize = 64 * KB;

fn field_bytes(i: u32) -> Vec<u8> {
    format!("field{i:06}").into_bytes()
}

// ---------------------------------------------------------------------
// entry_codec: raw encode/decode throughput
// ---------------------------------------------------------------------

fn entry_codec_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("entry_codec");
    group.throughput(Throughput::Elements(1));

    let long_str = vec![b'x'; KB];
    let cases: [(&str, EntryVal); 6] = [
        ("uint_imm", EntryVal::Uint(42)),       // 1-byte tag, no data
        ("uint_u16", EntryVal::Uint(1_000)),    // tag 251, 2-byte data
        ("uint_u64", EntryVal::Uint(u64::MAX)), // tag 254, 8-byte data
        ("str_8b", EntryVal::Str(b"abcdefgh")),
        ("str_64b", EntryVal::Str(&[b'x'; 64])),
        ("str_1kb", EntryVal::Str(&long_str)),
    ];

    for (name, val) in cases {
        let mut buf = [0u8; 2 * KB];
        let n = ziplist::encoded_len(&val);

        group.bench_function(format!("encode/{name}"), |b| {
            b.iter(|| {
                ziplist::encode_into(&val, &mut buf[..n]);
            })
        });

        ziplist::encode_into(&val, &mut buf[..n]);
        group.bench_function(format!("decode/{name}"), |b| {
            b.iter(|| ziplist::decode(&buf, 0).unwrap())
        });
    }
}

// ---------------------------------------------------------------------
// ops: per-op cost sweeps over nentry
// ---------------------------------------------------------------------

const NENTRY_SWEEP: [u32; 4] = [8, 64, 512, 4096];

/// Builds a `Hash` block with `nentry / 2` fields (`fieldNNNNNN` -> `"value"`)
/// in a fresh 64KB buffer.
fn populated_hash(nentry: u32, buf: &mut [u8]) -> HashMut<'_> {
    let mut h = HashMut::init(buf).unwrap();
    for i in 0..(nentry / 2) {
        h.hset(&field_bytes(i), b"value").unwrap();
    }
    h
}

/// Builds a `List` block with `nentry` short-string elements in a fresh
/// 64KB buffer.
fn populated_list(nentry: u32, buf: &mut [u8]) -> ListMut<'_> {
    let mut l = ListMut::init(buf).unwrap();
    for i in 0..nentry {
        l.push_back(&EntryVal::Str(&field_bytes(i))).unwrap();
    }
    l
}

/// Builds a `Zset` block with `nentry / 2` members (`fieldNNNNNN` with score
/// `i`) in a fresh 64KB buffer.
fn populated_zset(nentry: u32, buf: &mut [u8]) -> ZsetMut<'_> {
    let mut z = ZsetMut::init(buf).unwrap();
    for i in 0..(nentry / 2) {
        z.zadd(&field_bytes(i), i as u64).unwrap();
    }
    z
}

fn hget_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("ops/hget");
    group.throughput(Throughput::Elements(1));
    for nentry in NENTRY_SWEEP {
        let mut buf = vec![0u8; BUF_64K];
        let h = populated_hash(nentry, &mut buf);
        let view = h.view();
        let target = field_bytes(nentry / 4); // an existing, mid-range field
        group.bench_function(format!("nentry={nentry}"), |b| {
            b.iter(|| view.hget(&target).unwrap())
        });
    }
}

fn hset_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("ops/hset");
    group.throughput(Throughput::Elements(1));
    for nentry in NENTRY_SWEEP {
        // Setup rebuilds a fresh nentry-field hash each iteration (excluded
        // from the timed portion via `iter_batched`); the timed op updates
        // an already-existing field's value, exercising `pair_seek` at this
        // `nentry` without growing the block across iterations.
        let target = field_bytes(nentry / 4);
        group.bench_function(format!("nentry={nentry}"), |b| {
            b.iter_batched(
                || vec![0u8; BUF_64K],
                |mut buf| {
                    let mut h = populated_hash(nentry, &mut buf);
                    h.hset(&target, b"updated").unwrap();
                },
                BatchSize::SmallInput,
            )
        });
    }
}

fn push_back_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("ops/push_back");
    group.throughput(Throughput::Elements(1));
    for nentry in NENTRY_SWEEP {
        group.bench_function(format!("nentry={nentry}"), |b| {
            b.iter_batched(
                || vec![0u8; BUF_64K],
                |mut buf| {
                    let mut l = populated_list(nentry, &mut buf);
                    l.push_back(&EntryVal::Str(b"new")).unwrap();
                },
                BatchSize::SmallInput,
            )
        });
    }
}

fn zadd_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("ops/zadd");
    group.throughput(Throughput::Elements(1));
    for nentry in NENTRY_SWEEP {
        // A brand-new member: exercises zset.rs's linear `find_member` scan
        // (the module docs call out that member lookups can't binary-search
        // the score-sorted body), so this is the op most sensitive to
        // nentry among the four sweeps.
        let new_member = b"zzz-not-present".to_vec();
        group.bench_function(format!("nentry={nentry}"), |b| {
            b.iter_batched(
                || vec![0u8; BUF_64K],
                |mut buf| {
                    let mut z = populated_zset(nentry, &mut buf);
                    z.zadd(&new_member, u64::MAX / 2).unwrap();
                },
                BatchSize::SmallInput,
            )
        });
    }
}

criterion_group!(
    benches,
    entry_codec_benchmark,
    hget_benchmark,
    hset_benchmark,
    push_back_benchmark,
    zadd_benchmark,
);
criterion_main!(benches);
