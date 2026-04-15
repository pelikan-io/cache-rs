# segcache: Segment-Structured Cache Engine

## Overview

Segcache is a key-value cache storage engine that achieves 88% metadata reduction compared to Memcached by organizing items into fixed-size segments grouped by TTL. It was presented at [NSDI'21](https://www.usenix.org/conference/nsdi21/presentation/yang-juncheng) and described in a [blog post](https://pelikan.io/2021/segcache.html).

The engine provides a pluggable eviction policy interface. Storage mechanics — segment allocation, hash table lookup, TTL expiration, item packing — are shared across all policies. Adding a new eviction algorithm requires only the decision logic for selecting which segment to reclaim.

## Why Segments

Traditional caches (Memcached, Redis) store per-item metadata: pointers for hash chains, LRU links, timestamps, reference counts. This adds ~56 bytes per item — often more than the item itself for small objects.

Segcache lifts most metadata into shared structures:

- **Segment headers** (64 bytes each, stored in DRAM) track write offset, live bytes/items, TTL, pool classification, and doubly-linked list pointers. One header covers thousands of items.
- **Hash bucket slots** (8 bytes each) store a 12-bit tag, 8-bit frequency counter, 24-bit segment ID, and 20-bit offset. No per-item pointers.
- **Item headers** (5 bytes without magic, 9 with) store only key length (8 bits), value length (24 bits), and flags (8 bits). Defined in the shared [keyvalue](keyvalue.md) crate.

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

### Segment Header

Each header occupies exactly one cache line (64 bytes):

```
┌──────┬────────────┬────────────┬────────────┐
│  ID  │Write Offset│ Live Bytes │ Live Items │
│ 32b  │    32b     │    32b     │    32b     │
├──────┼────────────┼────────────┼────────────┤
│ Prev │    Next    │ Create At  │  Merge At  │
│ 32b  │    32b     │    32b     │    32b     │
├──────┼──┬──┬──┬───┴────────────┴────────────┤
│ TTL  │A │E │P │          Padding            │
│ 32b  │8b│8b│8b│                             │
├──────┴──┴──┴──┴─────────────────────────────┤
│                   Padding                    │
└──────────────────────────────────────────────┘
   A = Accessible, E = Evictable, P = Pool (Admission/Main)
```

The **pool** field (1 byte, from existing padding) classifies segments as `Admission` or `Main` for the S3-FIFO policy. Other policies leave all segments as `Main`.

## Item Layout

Each item within a segment uses the packed format from the [keyvalue](keyvalue.md) crate:

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

- Slot 0 is metadata: a shared CAS counter, chain length, and timestamp for frequency smoothing.
- Slots 1-7 hold items: a 12-bit tag (partial hash for fast rejection), an 8-bit approximate frequency counter, a 24-bit segment ID, and a 20-bit offset (8-byte aligned, so 20 bits covers 8 MB).
- Overflow chains link additional buckets when 7 slots aren't enough.

Lookups check the tag first (cache-friendly, as the entire bucket fits in one cache line), then verify the full key only on tag match.

The frequency counter is stored in the hash table slot, not in the item itself. This means frequency checks during eviction don't require touching item data — only the hash table is scanned.

## TTL Buckets and Expiration

Items are grouped into TTL buckets based on their TTL value. There are 1024 buckets organized in 4 tiers with increasing granularity:

| Tier | TTL Range | Bucket Width | Count |
|------|-----------|--------------|-------|
| 1 | 0 - 255s | 1 second | 256 |
| 2 | 256s - ~68 min | 4 seconds | 256 |
| 3 | ~68 min - ~4.5 hr | 16 seconds | 256 |
| 4 | ~4.5 hr - ~18 hr | 64 seconds | 256 |

Items with TTL beyond ~18 hours are mapped to the last bucket. Items with TTL = 0 (no expiration) go to bucket 0.

Expiration is **eager**: `expire()` walks each TTL bucket and checks if the head segment's TTL has elapsed. If so, the entire segment is freed — all items in it are expired in O(1) time. No per-item timers or lazy deletion needed. This behavior is shared by all eviction policies, including S3-FIFO.

## Eviction Policies

Eight policies, set at construction time. The storage layer is identical for all of them — only the segment selection strategy differs:

| Policy | Segment selection | Item-level scanning? |
|--------|-------------------|---------------------|
| `None` | — (inserts fail when full) | No |
| `Random` | Random segment | No |
| `RandomFifo` | Random TTL bucket → oldest segment | No |
| `Fifo` | Globally oldest segment | No |
| `Cte` | Segment closest to expiration | No |
| `Util` | Segment with fewest live bytes | No |
| `Merge` | Sequential segments in a TTL chain | Yes — frequency-based pruning |
| `S3Fifo` | Oldest admission-pool or main-pool segment | Yes — frequency-based promotion ([S3-Segcache](s3fifo.md)) |

The first six policies evict the entire selected segment — all items are dropped. `Merge` and `S3Fifo` are more sophisticated: they scan items within the segment and selectively copy high-value items to a fresh segment before freeing the source.

### Merge-Based Eviction

The `Merge` policy is segcache's original innovation. Instead of discarding an entire segment, it:

1. Takes N consecutive segments from a TTL bucket chain
2. Copies items with high enough frequency counters into a target segment
3. Discards items with low frequency (eviction) or dead items (compaction)
4. Returns freed segments to the free queue

Parameters: `max` (max segments per pass), `merge` (target merge count), `compact` (compaction threshold — segments below 1/N occupancy trigger compaction).

### S3-Segcache Eviction

The `S3Fifo` policy ([detailed description](s3fifo.md)) — referred to as **S3-Segcache** when describing the full configuration — introduces a two-pool architecture within the same segment infrastructure:

- **Admission pool**: Probationary. New items land here. When an admission segment is evicted, items with `freq > 0` are promoted (copied to a main segment), items with `freq == 0` are dropped and their key hashes added to a ghost queue.
- **Main pool**: Proven items. Eviction uses CLOCK-style second chance — items with `freq > 0` are copied to a fresh main segment, items with `freq == 0` are dropped.
- **Ghost queue**: A fixed-size set of key hashes. When a newly inserted key matches a ghost entry, it bypasses admission and goes directly to main.

The pool split is configured via `admission_ratio` (0.0–1.0, default 0.10). The exact number of admission-pool segments is computed at construction time (`round(total_segments * admission_ratio)`) and enforced as a hard cap on every insert via an O(1) counter check.

The pool distinction is a single byte in the segment header (using existing padding). The promotion/retention copying reuses the same `relink_item` + `copy_nonoverlapping` machinery that `Merge` uses for segment merging.

## Threading Model

Segcache is **single-threaded by design**. There are no locks, atomics, or concurrent access mechanisms. The `Segcache` struct requires `&mut self` for all operations including reads. This is intentional: cache workloads are typically partitioned across threads, with each thread owning its own cache instance.

The segment-based architecture is inherently concurrency-friendly for a future multi-threaded design:
- Writes are append-only within a segment (a thread can own a segment exclusively)
- Reads don't mutate storage (only frequency counters in the hash table)
- Reclamation is segment-granular (a single pointer swap)

## Feature Flags

| Feature | Effect |
|---------|--------|
| `magic` | Writes 0xDECAFBAD at the start of each item and segment for corruption detection |
| `debug` | Enables `magic` + exposes `items()` count and `check_integrity()` |
| `metrics` | (default) Exports counters/gauges via the `metriken` crate |

## Public API

```rust
use segcache::{Policy, Segcache};
use std::time::Duration;

const MB: usize = 1024 * 1024;

// Construction — pick any eviction policy
let mut cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::S3Fifo { admission_ratio: 0.10 })
    .build()?;

// Operations (identical regardless of policy)
cache.insert(b"key", b"value", None, Duration::from_secs(300))?;
let item = cache.get(b"key");
cache.delete(b"key");
cache.cas(b"key", b"new_value", None, Duration::from_secs(300), cas)?;
cache.wrapping_add(b"counter", 1)?;
cache.saturating_sub(b"counter", 1)?;

// Maintenance
cache.expire();  // eager TTL expiration
cache.clear();   // remove all items
```

The API is the same for all eviction policies. Only the `eviction(Policy::...)` builder call changes behavior.
