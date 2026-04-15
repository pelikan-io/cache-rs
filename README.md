# cache-rs

A collection of Rust implementations of state-of-the-art cache algorithms.

## Crates

| Crate | Description |
|-------|-------------|
| [**segcache**](docs/segcache.md) | Segment-structured cache with eager TTL expiration ([NSDI'21](https://www.usenix.org/conference/nsdi21/presentation/yang-juncheng)) |
| [**s3fifo**](docs/s3fifo.md) | S3-FIFO eviction: three static FIFO queues for scan-resistant caching ([SOSP'23](https://dl.acm.org/doi/10.1145/3600006.3613147)) |
| [**keyvalue**](docs/keyvalue.md) | Shared key-value item types (header, raw item, value) used by cache engines |
| [**datatier**](docs/datatier.md) | Byte storage pool abstraction (anonymous mmap, file-backed mmap, hybrid) |

## Architecture

```
┌───────────┐  ┌───────────┐
│  segcache │  │  s3fifo   │    cache engines
└─────┬─────┘  └─────┬─────┘
      │              │
      ▼              ▼
┌───────────┐  ┌───────────┐
│ keyvalue  │  │           │    shared types
└───────────┘  │           │
      ▲        │           │
      │        │           │
┌─────┴─────┐  │           │
│ datatier  │──┘           │    storage backends
└───────────┘              │
```

- **segcache** depends on both keyvalue (item types) and datatier (segment storage via mmap)
- **s3fifo** depends on keyvalue (item types) and uses a built-in slab allocator
- **keyvalue** has no external dependencies

## Building

```sh
cargo build --workspace
cargo test --workspace
```

## License

MIT OR Apache-2.0
