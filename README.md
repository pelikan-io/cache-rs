# cache-rs

Rust implementations of cache storage engines from [Pelikan](https://github.com/pelikan-io/pelikan).

## Crates

| Crate | Description |
|-------|-------------|
| [**segcache**](docs/segcache.md) | Segment-structured cache engine with pluggable eviction policies |
| [**cuckoo-cache**](docs/cuckoo.md) | Array-based cuckoo hash cache with fixed-size item slots |
| [**keyvalue**](docs/keyvalue.md) | Shared packed item types (`Value`, `RawItem`, `TinyItem`) |
| [**datatier**](docs/datatier.md) | Byte storage pool abstraction (anonymous mmap, file-backed mmap, hybrid) |

See [design](docs/design.md) for architecture details and eviction policy comparison.

## Quick Start

```rust
use segcache::{Policy, Segcache};
use std::time::Duration;

const MB: usize = 1024 * 1024;

let mut cache = Segcache::builder()
    .heap_size(64 * MB)
    .segment_size(1 * MB as i32)
    .hash_power(16)
    .eviction(Policy::S3Fifo { admission_ratio: 0.10 })
    .build()
    .expect("failed to create cache");

cache.insert(b"key", b"value", None, Duration::from_secs(300))?;
let item = cache.get(b"key").expect("not found");
assert_eq!(item.value(), b"value");
cache.delete(b"key");
```

## Building

```sh
cargo build --workspace
cargo test --workspace
```

## License

MIT OR Apache-2.0
