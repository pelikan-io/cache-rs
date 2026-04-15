# cache-rs

A collection of Rust implementations of state-of-the-art cache algorithms, built on a shared segment-structured storage engine.

## Crates

| Crate | Description |
|-------|-------------|
| [**segcache**](docs/segcache.md) | Segment-structured cache engine with pluggable eviction policies |
| [**keyvalue**](docs/keyvalue.md) | Shared packed item types (`Value`, `ItemHeader`, `RawItem`) |
| [**datatier**](docs/datatier.md) | Byte storage pool abstraction (anonymous mmap, file-backed mmap, hybrid) |

## Design

The central idea is **separation of storage from eviction**. The storage layer — segment allocation, TTL-based expiration, hash table lookup, item packing — is shared infrastructure. Eviction policies are pluggable strategies that decide *which* segment to reclaim when memory pressure occurs, without reimplementing any storage machinery.

```
                    ┌───────────────────────────────────────┐
                    │             segcache                   │
                    │                                       │
  eviction policy   │  ┌─────┬──────┬─────┬───────┬──────┐ │
  (pluggable)       │  │None │Random│ Fifo│ Merge │S3Fifo│ │
                    │  └──┬──┴──┬───┴──┬──┴───┬───┴──┬───┘ │
                    │     └─────┴──────┴──────┴──────┘     │
                    │              ▼                         │
  storage layer     │  ┌─────────────────────────────────┐ │
  (shared)          │  │  segments · TTL buckets · hash   │ │
                    │  │  table · item packing · CAS      │ │
                    │  └─────────────┬───────────────────┘ │
                    └────────────────┼──────────────────────┘
                                     │
              ┌──────────────────────┼──────────────────┐
              ▼                      ▼                  ▼
        ┌───────────┐         ┌───────────┐      ┌───────────┐
        │ keyvalue  │         │ datatier  │      │ metriken  │
        │ (items)   │         │ (mmap)    │      │ (metrics) │
        └───────────┘         └───────────┘      └───────────┘
```

This means adding a new eviction algorithm — like S3-FIFO — requires only the decision logic (~250 lines), not a new storage engine. The new policy automatically inherits:

- **Pre-allocated mmap'd heap** — zero per-item malloc
- **Eager TTL expiration** — O(1) per segment, no timers
- **Compact item headers** — 5 bytes per item (88% less than Memcached)
- **Cacheline-aligned hash table** — bulk chaining with 12-bit tags
- **CAS support, numeric operations, optional metadata**
- **Corruption detection** (magic feature), **metrics** (metriken)

## Eviction Policies

All policies operate at segment granularity. When memory runs out, the policy selects a segment to reclaim. What happens to the items inside depends on the policy:

| Policy | Selection | Item fate |
|--------|-----------|-----------|
| `None` | — | Inserts fail when full |
| `Random` | Random segment | All items dropped |
| `RandomFifo` | Random TTL bucket → oldest segment | All items dropped |
| `Fifo` | Globally oldest segment | All items dropped |
| `Cte` | Segment closest to expiration | All items dropped |
| `Util` | Segment with fewest live bytes | All items dropped |
| `Merge` | Sequential segments in a TTL chain | High-frequency items copied to target, low-frequency dropped |
| `S3Fifo` | Oldest small-pool or main-pool segment | [Frequency-based promotion/eviction](docs/s3fifo.md) — **S3-Segcache** |

`Merge` and `S3Fifo` are the sophisticated policies — they scan items within the evicted segment and selectively retain valuable ones by copying them elsewhere. The simpler policies discard everything in the segment.

## Quick Start

```rust
use segcache::{Policy, Segcache};
use std::time::Duration;

const MB: usize = 1024 * 1024;

// Create a 64 MB S3-Segcache
let mut cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::S3Fifo)
    .build()
    .expect("failed to create cache");

// Insert, get, delete
cache.insert(b"key", b"value", None, Duration::from_secs(300))?;
let item = cache.get(b"key").expect("not found");
assert_eq!(item.value(), b"value");
cache.delete(b"key");

// Eager TTL expiration
cache.expire();
```

## Building

```sh
cargo build --workspace
cargo test --workspace
```

## License

MIT OR Apache-2.0
