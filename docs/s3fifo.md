# S3-Segcache: Scan-Resistant Segment-Structured Cache

## Overview

S3-Segcache is segcache configured with `Policy::S3Fifo` — the S3-FIFO eviction algorithm operating on segcache's segment-structured storage engine. It uses two pools of segments — admission and main — plus a ghost queue of recently evicted key fingerprints to achieve scan-resistant caching with near-optimal hit ratios. Published at [SOSP'23](https://dl.acm.org/doi/10.1145/3600006.3613147) and described at [s3fifo.com](https://s3fifo.com/).

## The Algorithm

The original S3-FIFO paper describes three per-item FIFO queues. This implementation adapts the algorithm to operate on segments, keeping the same admission/promotion/eviction semantics while inheriting segcache's storage efficiency.

### Three Structures

```
Insert ──┐
         ▼
    ┌──────────┐    freq > 0    ┌──────────┐
    │Admission │ ──────────────▶│   Main   │
    │ Segments │   promote      │ Segments │
    │  (~10%)  │   (copy item)  │  (~90%)  │
    └────┬─────┘                └────┬─────┘
         │ freq == 0                 │ freq == 0
         │ drop item                 │ drop item
         ▼                          ▼
    ┌──────────┐               (discarded)
    │  Ghost   │
    │  Queue   │
    │ (hashes) │
    └──────────┘
```

**Admission-pool segments**: Probation area. New items are written to segments marked `Admission`. This pool is intentionally small — most items are accessed only once and should be evicted quickly without polluting the main cache.

**Main-pool segments**: Proven items. Segments marked `Main` hold items that demonstrated access frequency during their time in the admission pool, or items that matched a ghost entry on reinsertion.

**Ghost queue**: A fixed-capacity set of 64-bit key hashes stored as a `VecDeque` (FIFO eviction) paired with an `AHashSet` (O(1) lookup). Capacity is auto-sized to `max(1024, num_segments * 64)`.

### Pool Sizing

The split between admission and main is configured via `admission_ratio` (0.0–1.0) and enforced as a hard cap computed at construction time:

```
admission_cap = round(total_segments * admission_ratio)
```

For a 64 MB heap with 1 MB segments (64 segments) and `admission_ratio: 0.10`:
- Admission pool: 6 segments (6 MB)
- Main pool: 58 segments (58 MB)

The cap is enforced at insert time: before reserving space, if the target pool is at capacity, eviction is triggered. An `admission_count` counter (incremented on Main→Admission transitions, decremented when an Admission segment returns to the free queue) makes the capacity check O(1).

### Admission: Ghost-Guided Routing

On every `insert()`, segcache hashes the key and checks the ghost queue:

- **Ghost hit**: Item is written to a main-pool segment. The ghost entry is removed. This key was recently evicted from the admission pool with `freq == 0` but has reappeared — it deserves a second chance in main.
- **Ghost miss**: Item is written to an admission-pool segment (if the admission pool has room). It must prove itself by being accessed before its segment is evicted.

The pool label is set on the segment header. Segments within the same TTL bucket may have different pool labels — the pool is per-segment, not per-bucket.

### Eviction: Admission Pool

When memory pressure triggers eviction and an admission-pool segment is selected (oldest evictable admission segment across all TTL buckets):

1. A fresh segment is taken from the free queue and labeled `Main`
2. Every item in the admission segment is scanned:
   - **`freq > 0`**: Item is copied to the fresh main segment via `relink_item` + `copy_nonoverlapping` (same machinery as `Merge` eviction). The hash table entry is updated to point to the new location.
   - **`freq == 0`**: Item is dropped. Its key hash is added to the ghost queue.
3. The admission segment is cleared and returned to the free queue (`admission_count` decremented)

This is the key filtering step. Items that were never accessed during their time in the admission pool are discarded cheaply. Items that proved popular are promoted to main where they get a longer lifetime.

### Eviction: Main Pool

When no admission-pool segments are available for eviction (all have been drained or are immature), the oldest main-pool segment is selected. The logic mirrors the CLOCK second-chance algorithm:

1. A fresh segment is taken from the free queue and labeled `Main`
2. Every item in the evicted segment is scanned:
   - **`freq > 0`**: Item is copied to the fresh segment (second chance). Frequency is not explicitly reset here — the hash table's frequency smoothing handles decay over time.
   - **`freq == 0`**: Item is dropped permanently. No ghost entry.
3. The evicted segment is cleared and returned to the free queue

### Frequency Counters

S3-Segcache uses the same frequency counters already stored in segcache's hash table slots (8-bit approximate counters with probabilistic increment). No additional per-item metadata is needed. During eviction, `hashtable.get_freq(key, segment, offset)` reads the counter without touching item data.

## What S3-Segcache Inherits from Segcache

By implementing S3-FIFO as a policy within segcache rather than a standalone cache, it automatically gets:

| Feature | How it works |
|---------|-------------|
| **Pre-allocated mmap'd heap** | All segments live in a contiguous mmap region allocated at startup. Zero per-item malloc. |
| **Eager TTL expiration** | TTL buckets expire entire segments in O(1). Works identically for both admission and main segments. |
| **Compact item headers** | 5 bytes per item via the shared `keyvalue` crate. |
| **Bulk-chaining hash table** | Cacheline-aligned buckets with 12-bit tags. Frequency counters embedded in hash slots. |
| **CAS, numeric ops, optional data** | Full API surface — `cas()`, `wrapping_add()`, `saturating_sub()`, optional metadata. |
| **Corruption detection** | Magic bytes in items and segments (`magic` feature). |
| **Metrics** | All existing counters/gauges via `metriken`. |

## Usage

```rust
use segcache::{Policy, Segcache};

const MB: usize = 1024 * 1024;

let mut cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::S3Fifo { admission_ratio: 0.10 })
    .build()
    .expect("failed to create cache");
```

The API is identical to any other eviction policy. Only the builder's `.eviction()` call differs.

## Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `admission_ratio` | 0.10 | Fraction of segments for the admission pool (0.0–1.0). Lower values filter more aggressively but give items less time to prove themselves. Higher values provide more probation capacity for bursty workloads. |

The pool sizes are fixed at construction. At runtime, the `admission_count` counter enforces the cap in O(1) per insert — no scanning.

## When to Use S3-Segcache

**Best for:**
- Skewed popularity distributions (Zipf-like) — a small set of hot keys, a long tail of cold keys
- Scan-heavy workloads — sequential access patterns that would pollute an LRU cache
- One-hit wonders — workloads where many keys are accessed exactly once

**Consider alternatives when:**
- Uniform access patterns — `Fifo` or `Random` perform similarly with less overhead
- Very short TTLs dominate — TTL expiration handles most reclamation regardless of policy
- Write-heavy with frequent overwrites — `Merge` with compaction may reclaim dead bytes more efficiently

## Tradeoffs vs Other Segcache Policies

| Aspect | S3Fifo | Merge | Fifo |
|--------|--------|-------|------|
| Scan resistance | Strong (quick demotion via admission pool) | Moderate (frequency-based pruning) | None |
| Eviction cost | One segment scan + selective copy | Multi-segment merge + prune + compact | Immediate (drop entire segment) |
| Extra state | Ghost queue (~16 bytes/entry) + pool counter | None | None |
| Segment overhead | 1 byte per header (pool field) | None | None |
| Pool enforcement | O(1) counter check at insert | N/A | N/A |
| Hit ratio (skewed) | Near-optimal | Good | Fair |
| Hit ratio (uniform) | Good | Good | Good |
