# cache-rs

A collection of Rust implementations of state-of-the-art cache algorithms.

## Crates

| Crate | Description |
|-------|-------------|
| [**segcache**](docs/segcache.md) | Segment-structured cache with eager TTL expiration and pluggable eviction policies including S3-FIFO ([NSDI'21](https://www.usenix.org/conference/nsdi21/presentation/yang-juncheng), [SOSP'23](https://dl.acm.org/doi/10.1145/3600006.3613147)) |
| [**keyvalue**](docs/keyvalue.md) | Shared key-value item types (header, raw item, value) used by cache engines |
| [**datatier**](docs/datatier.md) | Byte storage pool abstraction (anonymous mmap, file-backed mmap, hybrid) |

## Architecture

```
┌───────────┐
│  segcache │    cache engine (Policy::Random, Fifo, Merge, S3Fifo, ...)
└─────┬─────┘
      │
      ▼
┌───────────┐
│ keyvalue  │    shared item types
└───────────┘
      ▲
      │
┌─────┴─────┐
│ datatier  │    storage backends (mmap, file-backed)
└───────────┘
```

## Eviction Policies

| Policy | Strategy |
|--------|----------|
| `None` | No eviction; inserts fail when full |
| `Random` | Evict a random segment |
| `RandomFifo` | Random TTL bucket, evict oldest segment |
| `Fifo` | Evict the globally oldest segment |
| `Cte` | Evict the segment closest to expiration |
| `Util` | Evict the least utilized segment |
| `Merge` | Merge segments, keeping high-frequency items |
| `S3Fifo` | Two-pool (small/main) with ghost queue for scan-resistant eviction |

## Building

```sh
cargo build --workspace
cargo test --workspace
```

## License

MIT OR Apache-2.0
