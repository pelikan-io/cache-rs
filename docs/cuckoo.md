# cuckoo-cache: Array-Based Cuckoo Hash Cache

## Overview

Cuckoo-cache is a key-value cache that stores items inline in a flat array of fixed-size slots, using cuckoo hashing with D=4 candidate positions per key. The design is based on the [cuckoo storage engine](https://github.com/pelikan-io/pelikan) from Pelikan.

Where segcache is optimized for high item counts with variable-size items and eager TTL expiration, cuckoo-cache targets workloads with small, uniform items where O(1) worst-case lookup and bounded insertion latency matter most.

## How Cuckoo Hashing Works

Each key is hashed with four independent hash functions, producing four candidate slot positions. An item can only live in one of its four candidate slots.

```
Key "coffee" ──▶ H₀ ──▶ slot 42
               ──▶ H₁ ──▶ slot 187
               ──▶ H₂ ──▶ slot 903
               ──▶ H₃ ──▶ slot 511
```

- **Get**: Check all 4 positions. Return the item if found, `None` otherwise.
- **Insert**: Check all 4 positions for existing key (update), empty slot, or expired slot. If none available, try displacement, then evict.
- **Delete**: Check all 4 positions, clear the matching slot.

All operations are O(1) with a small constant (at most 4 slot accesses for a hit, plus displacement work on insert).

## Item Layout

Each slot uses the [`TinyItem`](keyvalue.md#tinyitem) format from the keyvalue crate — a 6-byte packed header followed directly by key and value bytes:

```
Slot (64 bytes default):
┌──────────┬──────┬──────┬──────────┬──────────┐
│  EXPIRE  │ KLEN │ VLEN │   KEY    │  VALUE   │
│  (u32)   │ (u8) │ (u8) │          │          │
│ 4 bytes  │ 1 b  │ 1 b  │          │          │
└──────────┴──────┴──────┴──────────┴──────────┘
```

- `expire == 0`: empty slot (all bytes zeroed)
- `expire == u32::MAX`: item with no expiry (TTL = 0)
- `expire > 0 && expire < u32::MAX`: absolute expiry as seconds since cache creation
- `vlen == 0`: value is a `u64` integer (8 bytes stored big-endian)
- `vlen > 0`: value is a byte string of that length

The per-item overhead is just 6 bytes. For the default 64-byte slot, this leaves 58 bytes for key + value data.

## Hashing

Four independent hash functions are constructed from `ahash::RandomState` with four distinct fixed seed sets — analogous to the Murmur3-with-different-IVs approach used in the C implementation. The fixed seeds ensure deterministic slot assignment: the same key always maps to the same four candidate positions.

```rust
const SEEDS: [[u64; 4]; 4] = [
    [0x3ac5_d673, 0x6d78_39d0, 0x2b58_1cf5, 0x4dd2_be0a],
    [0x9e37_79b9, 0x517c_c1b7, 0x27d4_eb2f, 0x3c6e_f372],
    [0xdead_beef, 0xcafe_babe, 0x1234_5678, 0xfeed_face],
    [0xa076_1d64, 0xe703_7ed1, 0x8ebc_6af0, 0x5899_65cd],
];
```

## Displacement

When all four candidate positions are occupied and none are empty or expired, the insertion algorithm attempts to make room by **displacing** an existing item to one of *its* alternative positions. This cascades up to `max_displace` levels deep (default 2).

```
Insert key K — all 4 candidates occupied:

  K's candidate slot 42 holds item A
  A's alternative slot 710 is empty
  → Move A from slot 42 to slot 710
  → Write K into slot 42

If A has no empty alternatives either, try moving one of A's
neighbors to *its* alternative (depth 2), and so on up to max_displace.
```

The algorithm prefers direct moves to empty or expired slots. If no displacement path exists within the depth limit, the insertion falls through to eviction.

## Eviction

When displacement fails, an item must be evicted. The eviction policy selects one of the four candidate positions:

| Policy | Selection |
|--------|-----------|
| `Random` | Random candidate (uniform) |
| `Expire` | Candidate with nearest expiration time |

The selected item is cleared and the new item takes its slot.

## Lazy Expiration

Unlike segcache's eager segment-based expiration, cuckoo-cache expires items **lazily**. Expired items are discovered and cleared during normal operations:

- **Get**: If the matching key has expired, clear it and return `None`.
- **Insert**: Pass 3 of the insertion algorithm checks all candidates for expired slots before attempting displacement or eviction.
- **Displacement**: Expired items encountered during displacement chains are cleared and treated as free slots.

Items that are never accessed again remain in their slots until another operation discovers them.

## Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `item_size` | 64 | Bytes per slot. Key + value + 6 bytes header must fit. |
| `nitem` | 1024 | Total number of slots (maximum item capacity). |
| `max_displace` | 2 | Maximum displacement chain depth. Higher values reduce evictions but increase worst-case insertion cost. |
| `policy` | `Random` | Eviction policy: `Random` or `Expire`. |
| `max_ttl` | 2,592,000 | Maximum TTL in seconds (30 days). Higher values are clamped. |

Total memory usage is `item_size * nitem` bytes for the data array, plus a small fixed overhead for the hash builders and metadata.

## Feature Flags

| Feature | Effect |
|---------|--------|
| `magic` | Forwards to `keyvalue/magic` — writes 0xDECAFBAD in item headers |
| `debug` | Enables `magic` + exposes `items()` count |
| `metrics` | (default) Exports counters/gauges via the `metriken` crate |

## Metrics

All metrics carry `metadata = { engine = "cuckoo" }`.

| Name | Type | Description |
|------|------|-------------|
| `get` | Counter | Total get operations |
| `get_key_hit` | Counter | Gets that found the key |
| `get_key_miss` | Counter | Gets that did not find the key |
| `insert` | Counter | Total insert operations |
| `insert_ex` | Counter | Inserts that failed (item oversized) |
| `update` | Counter | In-place updates of existing keys |
| `delete` | Counter | Total delete operations |
| `displace` | Counter | Items displaced during insertion |
| `item_evict` | Counter | Items evicted to make room |
| `item_expire` | Counter | Items lazily expired during operations |
| `item_current` | Gauge | Current number of live items |
| `item_key_byte` | Gauge | Current total key bytes stored |
| `item_val_byte` | Gauge | Current total value bytes stored |
| `item_data_byte` | Gauge | Current total data bytes (key + value) |

## Public API

```rust
use cuckoo_cache::{CuckooCache, Policy};
use std::time::Duration;

// Construction
let mut cache = CuckooCache::builder()
    .nitem(65536)
    .item_size(64)
    .max_displace(2)
    .policy(Policy::Random)
    .build();

// Operations
cache.insert(b"key", b"value", Duration::from_secs(300))?;
let item = cache.get(b"key");
cache.delete(b"key");
cache.wrapping_add(b"counter", 1)?;
cache.saturating_sub(b"counter", 1)?;

// Reset
cache.clear();
```

## When to Use Cuckoo-Cache

**Best for:**
- Small, uniform items that fit in a fixed slot size (e.g., session tokens, feature flags, counters)
- Workloads where predictable O(1) access time matters
- Memory-constrained environments where the fixed allocation is an advantage

**Consider segcache when:**
- Item sizes vary widely — segcache packs variable-size items efficiently in segments
- You need eager TTL expiration — segcache expires entire segments in O(1)
- You need CAS support — segcache provides per-bucket CAS counters
- High item counts with low per-item overhead — segcache's 5-byte headers beat cuckoo's fixed slot waste for small items

## Tradeoffs vs Segcache

| Aspect | cuckoo-cache | segcache |
|--------|-------------|----------|
| Item size | Fixed per slot | Variable (packed in segments) |
| Item header | 6 bytes (`TinyItem`) | 5 bytes (`ItemHeader`) |
| Lookup | O(1), check 4 slots | O(1), hash table + segment read |
| Insertion worst case | Displacement chain (bounded) | Segment allocation + possible eviction |
| Expiration | Lazy (on access) | Eager (segment-granular) |
| Memory waste | Unused bytes in slots | Minimal (items packed tightly) |
| Eviction granularity | Single item | Entire segment |
| CAS | Not supported | Per-bucket CAS counter |
