# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

cache-rs is a collection of Rust cache storage engines originally from [Pelikan](https://github.com/pelikan-io/pelikan). It provides high-performance, memory-efficient caching with pluggable eviction policies and minimal per-item metadata overhead.

## Build Commands

```bash
cargo build --workspace            # Build all crates
cargo test --workspace             # Run all tests
cargo test -p segcache             # Test a single crate
cargo test -p segcache --features debug  # Test with debug features (exposes items() count, check_integrity())
cargo clippy --all-targets --all-features -- -D warnings  # Lint (CI enforces -D warnings)
cargo fmt --all --check            # Format check
cargo bench -p segcache            # Run benchmarks (criterion, 30s measurement)
```

## Workspace Structure

Four crates with clear dependency flow: **segcache** and **cuckoo-cache** are cache engines that depend on **keyvalue** (shared item types) and optionally **datatier** (storage backends).

### keyvalue — Packed Item Types

Defines `Value`/`OwnedValue` enums (bytes or u64) and two item layouts:

- **RawItem**: Used by segcache. 5-byte header (9 with `magic` feature). Variable-size keys/values up to 16MB. Stored as `*mut u8` pointer into segment memory.
- **TinyItem**: Used by cuckoo-cache. 6-byte fixed header. Keys and values limited to 255 bytes each. Expiration embedded in header (`0` = empty slot, `u32::MAX` = no expiry).

### datatier — Storage Pool Abstraction

`Datapool` trait with three implementations:
- `Memory`: Anonymous mmap with page prefaulting (standard use case)
- `MmapFile`: File-backed mmap with blake3 checksum header (persistent memory/DAX)
- `FileBackedMemory`: DRAM via anon mmap + periodic page flush to file (NVMe durability)

### segcache — Segment-Structured Cache

Append-only segments (64-byte headers) with bulk-chaining hash table (64 bytes per bucket = one cache line, 8 slots). Items are 8-byte aligned within segments. TTL buckets (4 tiers, 1024 total) enable O(1) eager expiration of entire segments.

Eight eviction policies set at construction time via `Policy` enum. Simple policies (Random, Fifo, Cte, Util) drop entire segments. Sophisticated policies (Merge, S3Fifo) scan items and use frequency counters in hash slots to selectively copy high-value items.

All reads require `&mut self` because lookups update frequency counters in the hash table. This is intentional — workloads partition across threads with each owning a cache instance.

### cuckoo-cache — Fixed-Slot Cuckoo Hash

D=4 cuckoo hashing with four independent ahash builders (deterministic seeds). Each key maps to exactly 4 candidate slots. Fixed-size slots (`nitem * item_size` bytes allocated upfront). Displacement cascades up to `max_displace` levels before eviction. Lazy expiration (items expire on access, not proactively). Two eviction policies: `Random` and `Expire` (nearest expiration).

## Key Patterns

**Builder pattern**: Both engines use `::builder()` with fluent chaining, terminated by `.build()`.

**Feature flags** (shared across engines):
- `magic`: Enables 0xDECAFBAD corruption-detection bytes in item headers
- `debug`: Enables `magic` + exposes `items()` count and `check_integrity()`
- `metrics` (default): Exports counters/gauges via `metriken` crate with `metadata = { engine = "segcache" | "cuckoo" }`

**Error types**: `thiserror`-derived enums (`SegcacheError`, `CuckooCacheError`).

**Time**: Uses `clocksource::coarse::{Duration, Instant}` throughout, not `std::time`.

## CI

Runs on Ubuntu, macOS, and Windows. Enforces `clippy -D warnings` and `cargo fmt --check`.
