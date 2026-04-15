# S3-FIFO: Scan-Resistant Eviction Policy

## Overview

S3-FIFO is available as `Policy::S3Fifo` within segcache. It uses two pools of segments — small and main — plus a ghost queue of recently evicted key fingerprints to achieve scan-resistant caching with near-optimal hit ratios. Published at [SOSP'23](https://dl.acm.org/doi/10.1145/3600006.3613147) and described at [s3fifo.com](https://s3fifo.com/).

## How It Works

```
Insert ──┐
         ▼
    ┌──────────┐    freq > 0    ┌──────────┐
    │  Small   │ ──────────────▶│   Main   │
    │ Segments │   promote      │ Segments │
    │  (~10%)  │   (copy)       │  (~90%)  │
    └────┬─────┘                └────┬─────┘
         │ freq == 0                 │ freq == 0
         │ evict segment             │ evict segment
         ▼                          ▼
    ┌──────────┐               (discarded)
    │  Ghost   │
    │  Queue   │
    │ (hashes) │
    └──────────┘
```

**Small-pool segments**: New items land here. When a small segment is evicted under memory pressure, each item is checked:
- `freq > 0` → item is copied to a main-pool segment (promotion)
- `freq == 0` → item is dropped, key hash added to ghost queue

**Main-pool segments**: Items that proved themselves. When a main segment is evicted, CLOCK-style second chance applies:
- `freq > 0` → item is copied to a fresh main segment (retained)
- `freq == 0` → item is dropped permanently

**Ghost queue**: A ring of key hashes. When a new insert's key hash matches a ghost entry, the item bypasses small and goes directly to a main-pool segment — it was evicted prematurely last time.

## Segment-Level Operation

Unlike the per-item description in the paper, this implementation operates at segment granularity, consistent with segcache's architecture:

- Segments are labeled `Small` or `Main` via a pool field in the segment header
- Eviction processes an entire segment at a time
- Promotion copies individual items from a small segment to a main segment (reusing segcache's `copy_into` / `relink_item` machinery)
- TTL expiration works identically to other policies — entire segments are expired eagerly via TTL buckets

## Usage

```rust
use segcache::{Policy, Segcache};

const MB: usize = 1024 * 1024;

let cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::S3Fifo)
    .build()
    .expect("failed to create cache");
```

## When to Use S3-FIFO

S3-FIFO is best suited for workloads with:
- **Skewed popularity** — a small fraction of keys receive most accesses
- **Scan traffic** — sequential scans that would pollute an LRU cache
- **One-hit wonders** — many keys accessed exactly once

For workloads with uniform access or very short TTLs, `Policy::Merge` or `Policy::Fifo` may perform equally well with lower overhead (no promotion copying).
