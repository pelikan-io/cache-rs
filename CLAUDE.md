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
- `fault-injection` (segcache, test-only): Exposes `segcache::fault`, which forces `Segment::copy_into`'s relink CAS to lose so its otherwise-unreachable `RelinkFailure` abort path can be tested. Deliberately not default and **not** implied by `debug` — `debug` is observational, this changes behaviour. Never enable it outside tests.

**Error types**: `thiserror`-derived enums (`SegcacheError`, `CuckooCacheError`).

**Time**: Uses `clocksource::coarse::{Duration, Instant}` throughout, not `std::time`.

⚠️ **Two clocks, one type name.** `crate::Instant` is `clocksource::coarse::Instant` — **1-second resolution**. `std::time::Instant` is nanosecond-resolution. The name gives no hint which you have, and picking the wrong one fails *silently* rather than loudly.

- **Deadlines and item lifetimes → coarse.** `create_at`, `ttl`, `remaining_ttl`, `expiry_info`, `AtomicInstant`, TTL-bucket cutoffs, and the once-per-tick debounces. This is what the coarse clock is *for*: cheap, and item-lifetime arithmetic depends on its whole-second grid. Do not "improve" these to `std::time`.
- **Measuring how long an operation took → `std::time::Instant`.** A sub-second duration measured on the coarse clock truncates *both* endpoints to a second boundary, so `elapsed()` returns either `0` or exactly `1_000_000_000` ns — a random value, not a small one. That is worse than a dead counter: it looks like a plausible latency. This is exactly how `clear_time`/`expire_time`/`evict_time` were broken (#75), and #73 is the same trap in the TTL direction.

At a measurement site, import the nanosecond clock under an explicit alias (`use std::time::Instant as StdInstant;`) so the distinction is visible in the code rather than depending on which `Instant` happens to be in scope.

## CI

Runs on Ubuntu, macOS, and Windows. Enforces `clippy -D warnings` and `cargo fmt --check`.
