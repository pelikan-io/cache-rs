# s3fifo: S3-FIFO Cache Engine

## Overview

S3-FIFO is a cache eviction algorithm that uses three static FIFO queues to achieve near-optimal hit ratios with minimal overhead. It was published at [SOSP'23](https://dl.acm.org/doi/10.1145/3600006.3613147) and described at [s3fifo.com](https://s3fifo.com/). The key insight: most objects are accessed only once. By quickly demoting these one-hit wonders through a small probationary queue, the algorithm keeps the main cache populated with genuinely popular items.

## The Three Queues

```
Insert ──┐
         ▼
    ┌──────────┐    freq > 0    ┌──────────┐
    │  Small   │ ──────────────▶│   Main   │
    │  FIFO    │    promote     │   FIFO   │
    │  (~10%)  │                │  (~90%)  │
    └────┬─────┘                └────┬─────┘
         │ freq == 0                 │ freq == 0
         │ evict                     │ evict
         ▼                          ▼
    ┌──────────┐               (discarded)
    │  Ghost   │
    │  FIFO    │
    │ (hashes) │
    └──────────┘
```

**Small FIFO** (~10% of capacity): New items land here. Items that are never accessed again are evicted cheaply without polluting the main cache.

**Main FIFO** (~90% of capacity): Items promoted from the small queue after being accessed at least once. Items here have proven popularity.

**Ghost FIFO**: Stores only the hashes (fingerprints) of items evicted from small. When a key is reinserted and its hash is found in the ghost queue, the item goes directly to main — it's been seen before and was evicted prematurely.

## Eviction Rules

Each item carries a 2-bit frequency counter (0–3), incremented on every `get()`.

**Evicting from Small** (head of queue):
- `freq > 0` → promote to tail of main, reset freq to 0
- `freq == 0` → evict, add hash to ghost queue

**Evicting from Main** (head of queue):
- `freq > 0` → reinsert at tail of main, reset freq to 0
- `freq == 0` → evict permanently

This gives items in main a second chance proportional to their access frequency, similar to CLOCK, but without the overhead of a circular buffer or random access.

## Why FIFO Queues

Three properties make FIFO queues ideal here:

1. **O(1) operations**: push to tail, pop from head. No pointer chasing.
2. **No per-item metadata beyond the counter**: No LRU links, no timestamps.
3. **Scan-resistant**: A sequential scan fills the small queue, but items with `freq == 0` are evicted before reaching main. The main cache is unaffected.

## Internal Architecture

### Item Storage

Items are stored in a **slab allocator** — a `Vec` of slots with a free list. Each slot holds a heap-allocated byte buffer with the same packed layout used by segcache (via the shared `keyvalue` crate):

```
[ItemHeader (5 or 9 bytes)][optional][key][value]
```

Slots also carry per-item metadata for the eviction algorithm:

| Field | Size | Description |
|-------|------|-------------|
| `hash` | 8 bytes | Cached key hash (used for ghost queue + hashtable) |
| `freq` | 1 byte | Access frequency counter (0–3) |
| `queue` | 1 byte | Which FIFO the item belongs to (Small or Main) |
| `expire_at` | 4 bytes | TTL expiry (seconds since cache creation, 0 = never) |
| `deleted` | 1 byte | Tombstone flag for lazy queue cleanup |

### Hash Table

Adapted from segcache's bulk-chaining design. Each bucket is 64 bytes (one cache line) with 8 slots. Slot 0 stores bucket metadata (CAS counter, chain length). Slots 1–7 store item references:

```
Item Info (64 bits):
┌──────────────────────────────┬──────────────────────────────┐
│             TAG              │          SLAB INDEX           │
│            32 bit            │            32 bit             │
└──────────────────────────────┴──────────────────────────────┘
```

The 32-bit tag (vs segcache's 12-bit) reduces false-positive key comparisons. The 32-bit slab index supports up to ~4 billion items.

### Ghost Queue

A `VecDeque<u64>` for FIFO ordering paired with an `AHashSet<u64>` for O(1) membership tests. Capacity is auto-sized to approximate the number of items the small queue can hold. When a hash is found in the ghost set during insertion, the item bypasses small and goes directly to main.

### Lazy Deletion

When `delete()` is called, the item is removed from the hash table and marked as a tombstone in the slab. The FIFO queue entry is left in place and cleaned up when it naturally reaches the head during eviction. This keeps `delete()` O(1) without requiring a linear scan of the queue.

## Threading Model

Like segcache, s3fifo is **single-threaded by design**. All methods take `&mut self`. No locks, no atomics.

## Feature Flags

| Feature | Effect |
|---------|--------|
| `magic` | Enables item magic bytes (0xDECAFBAD) for corruption detection |
| `debug` | Enables `magic` |
| `metrics` | (default) Exports counters/gauges via the `metriken` crate |

## Public API

```rust
// Construction
let cache = S3Fifo::builder()
    .heap_size(64 * MB)
    .hash_power(16)
    .small_queue_ratio(0.10)
    .build()?;

// Operations
cache.insert(key, value, optional, ttl)?;
let item = cache.get(key);
cache.delete(key);
cache.cas(key, value, optional, ttl, cas_value)?;
cache.wrapping_add(key, delta)?;
cache.saturating_sub(key, delta)?;

// Maintenance
cache.expire();  // scan and remove expired items
cache.clear();   // remove all items
```

## Differences from Segcache

| Aspect | Segcache | S3-FIFO |
|--------|----------|---------|
| Eviction unit | Entire segment (~1 MB) | Individual item |
| Item storage | Packed in fixed-size segments | Slab with per-item heap allocation |
| TTL expiration | Eager, O(1) per segment via TTL buckets | Lazy on access + scan via `expire()` |
| Frequency counter | 8-bit probabilistic (in hash table) | 2-bit simple (in slab, cap at 3) |
| Eviction policy | Configurable (7 policies) | Fixed: S3-FIFO algorithm |
| Memory overhead | ~5 bytes/item (segment amortization) | ~24 bytes/item (slab + queue metadata) |
| Best for | Large working sets, uniform TTLs | Skewed popularity, scan-heavy workloads |
