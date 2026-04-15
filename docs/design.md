# Design

The central idea is **separation of storage from eviction**. The storage layer — segment allocation, TTL-based expiration, hash table lookup, item packing — is shared infrastructure. Eviction policies are pluggable strategies that decide *which* segment to reclaim when memory pressure occurs, without reimplementing any storage machinery.

```
                  ┌─────────────────────────────────────┐
                  │             segcache                 │
                  │                                     │
  eviction        │  ┌─────┬──────┬─────┬─────┬──────┐ │
  policy          │  │None │Random│Fifo │Merge│S3Fifo│ │
  (pluggable)     │  └──┬──┴──┬───┴──┬──┴──┬──┴──┬───┘ │
                  │     └─────┴──────┴─────┴─────┘     │
                  │               ▼                     │
  storage         │  ┌───────────────────────────────┐  │
  layer           │  │ segments · TTL buckets · hash │  │
  (shared)        │  │ table · item packing · CAS    │  │
                  │  └─────────────┬─────────────────┘  │
                  └────────────────┼────────────────────┘
                                   │
              ┌────────────────────┼────────────────┐
              ▼                    ▼                ▼
      ┌───────────┐         ┌───────────┐      ┌───────────┐
      │ keyvalue  │         │ datatier  │      │ metriken  │
      │ (items)   │         │ (mmap)    │      │ (metrics) │
      └───────────┘         └───────────┘      └───────────┘
```

This means adding a new eviction algorithm — like S3-FIFO — requires only the decision logic (~250 lines), not a new storage engine. The new policy automatically inherits:

- **Pre-allocated mmap'd heap** — zero per-item malloc
- **Eager TTL expiration** — O(1) per segment, no timers
- **Compact item headers** — 5–6 bytes per item (88% less than Memcached)
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
| `S3Fifo` | Oldest admission-pool or main-pool segment | [Frequency-based promotion/eviction](s3fifo.md) — **S3-Segcache** |

`Merge` and `S3Fifo` are the sophisticated policies — they scan items within the evicted segment and selectively retain valuable ones by copying them elsewhere. The simpler policies discard everything in the segment.
