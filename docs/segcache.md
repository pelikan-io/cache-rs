# segcache: Segment-Structured Cache Engine

## Overview

Segcache is a key-value cache storage engine that achieves 88% metadata reduction compared to Memcached by organizing items into fixed-size segments grouped by TTL. It was presented at [NSDI'21](https://www.usenix.org/conference/nsdi21/presentation/yang-juncheng) and described in a [blog post](https://pelikan.io/2021/segcache.html).

## Why Segments

Traditional caches (Memcached, Redis) store per-item metadata: pointers for hash chains, LRU links, timestamps, reference counts. This adds ~56 bytes per item -- often more than the item itself for small objects.

Segcache lifts most metadata into shared structures:

- **Segment headers** (64 bytes each, stored in DRAM) track write offset, live bytes/items, TTL, and doubly-linked list pointers. One header covers thousands of items.
- **Hash bucket slots** (8 bytes each) store a 12-bit tag, 8-bit frequency counter, 24-bit segment ID, and 20-bit offset. No per-item pointers.
- **Item headers** (5 bytes without magic, 9 with) store only key length (8 bits), value length (24 bits), and flags (8 bits).

The result: ~5 bytes of per-item overhead vs ~56 bytes in Memcached.

## Segment Design

Segments are fixed-size (default 1 MB), append-only byte regions. Items are written sequentially with 8-byte alignment. A segment is "owned" by one TTL bucket at a time and holds only items with similar TTLs.

```
Segment (1 MB):
┌─────────┬─────────┬─────────┬─── ─── ───┬────────────┐
│  Item 0 │  Item 1 │  Item 2 │    ...    │  Free      │
│ (hdr+kv)│ (hdr+kv)│ (hdr+kv)│           │  Space     │
└─────────┴─────────┴─────────┴─── ─── ───┴────────────┘
                                           ^
                                      write_offset
```

When a segment is full, a new one is taken from the free queue and linked into the TTL bucket's chain.

## Item Layout

Each item within a segment:

```
┌──────────┬──────────┬───────────────┬────────┬───────┬──────────┐
│  Magic?  │  VLen:24 │    KLen:8     │ Flags  │  Key  │  Value   │  Optional
│ (4 bytes)│          │               │ (1 b)  │       │          │  data
└──────────┴──────────┴───────────────┴────────┴───────┴──────────┘
```

The magic field (0xDECAFBAD) is only present with the `magic` feature. Flags pack a typed-value bit, padding, and 6-bit optional data length. Total header: 5 bytes (or 9 with magic), always 8-byte aligned within the segment.

## Hash Table: Bulk Chaining

The hash table uses a design called **bulk chaining**. Each primary bucket is exactly 64 bytes (one cache line) containing 8 slots:

```
HashBucket (64 bytes = 1 cache line):
┌─────────────────────────────────────────────────────────┐
│ Slot 0: Bucket Info (CAS:32, chain_len:8, timestamp:16) │
│ Slot 1-7: Item Info  (tag:12, freq:8, seg_id:24, off:20)│
└─────────────────────────────────────────────────────────┘
```

- Slot 0 is metadata: a shared CAS counter, chain length, and timestamp.
- Slots 1-7 hold items: a 12-bit tag (partial hash for fast rejection), an 8-bit approximate frequency counter, a 24-bit segment ID, and a 20-bit offset (8-byte aligned, so 20 bits covers 8 MB).
- Overflow chains link additional buckets when 7 slots aren't enough.

Lookups check the tag first (cache-friendly, as the entire bucket fits in one cache line), then verify the full key only on tag match.

## TTL Buckets and Expiration

Items are grouped into TTL buckets based on their TTL value. There are 1024 buckets organized in 4 tiers with increasing granularity:

| Tier | TTL Range | Bucket Width | Count |
|------|-----------|--------------|-------|
| 1 | 0 - 255s | 1 second | 256 |
| 2 | 256s - ~68 min | 4 seconds | 256 |
| 3 | ~68 min - ~4.5 hr | 16 seconds | 256 |
| 4 | ~4.5 hr - ~18 hr | 64 seconds | 256 |

Items with TTL beyond ~18 hours are mapped to the last bucket. Items with TTL = 0 (no expiration) go to bucket 0.

Expiration is **eager**: `expire()` walks each TTL bucket and checks if the head segment's TTL has elapsed. If so, the entire segment is freed -- all items in it are expired in O(1) time. No per-item timers or lazy deletion needed.

## Eviction Policies

Seven policies, set at construction time:

| Policy | Strategy |
|--------|----------|
| `None` | No eviction; inserts fail when full |
| `Random` | Evict a random segment |
| `RandomFifo` | Pick a random TTL bucket (weighted by segment count), evict its oldest segment |
| `Fifo` | Evict the globally oldest segment |
| `Cte` | Evict the segment closest to expiration |
| `Util` | Evict the segment with the fewest live bytes |
| `Merge` | Merge multiple segments, keeping high-frequency items (see below) |

### Merge-Based Eviction

The `Merge` policy is segcache's key innovation. Instead of discarding an entire segment, it:

1. Takes N consecutive segments from a TTL bucket chain
2. Copies items with high enough frequency counters into a target segment
3. Discards items with low frequency (eviction) or dead items (compaction)
4. Returns freed segments to the free queue

Parameters: `max` (max segments per pass), `merge` (target merge count), `compact` (compaction threshold -- segments below 1/N occupancy trigger compaction).

## Threading Model

Segcache is **single-threaded by design**. There are no locks, atomics, or concurrent access mechanisms. The `Segcache` struct requires `&mut self` for all operations including reads. This is intentional: cache workloads are typically partitioned across threads, with each thread owning its own cache instance.

## Feature Flags

| Feature | Effect |
|---------|--------|
| `magic` | Writes 0xDECAFBAD at the start of each item and segment for corruption detection |
| `debug` | Enables `magic` + exposes `items()` count and `check_integrity()` |
| `metrics` | (default) Exports counters/gauges via the `metriken` crate |

## Public API

```rust
// Construction
let cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::Random)
    .build()?;

// Operations
cache.insert(key, value, optional, ttl)?;
let item = cache.get(key);
cache.delete(key);
cache.cas(key, value, optional, ttl, cas_value)?;
cache.wrapping_add(key, delta)?;
cache.saturating_sub(key, delta)?;

// Maintenance
cache.expire();  // eager TTL expiration
cache.clear();   // remove all items
```
