//! `clear_time`, `expire_time` and `evict_time` measure REAL elapsed time.
//!
//! These three counters time sub-millisecond operations. They were originally
//! measured with `crate::Instant` — `clocksource::coarse::Instant`, which has
//! **1-second resolution** — so both endpoints truncated to a second boundary
//! and `elapsed()` could only ever return `0` (the operation fit inside one
//! coarse second) or exactly `1_000_000_000` ns (it straddled a tick). That is
//! worse than a dead counter: `evict_time` in particular accumulated a spurious
//! full second per tick-straddling eviction, so `evict_time / segment_evict`
//! looked like a latency and was a random walk (issue #75).
//!
//! WHY THE BOUNDS BELOW ARE AIRTIGHT AGAINST A REGRESSION: a coarse-clock
//! measurement can only contribute `0` or `1e9` ns, so any coarse-clocked total
//! lands in `{0, 1e9, 2e9, ...}`. Asserting `0 < delta < 1e9` rejects every one
//! of those: a total of zero fails the floor, and any total that recorded a
//! tick fails the ceiling. A real nanosecond clock lands comfortably inside
//! (single-digit milliseconds for a full sweep), so there is no flakiness
//! window between the two bounds. Do not relax either bound — dropping the
//! ceiling is what would let the coarse clock back in unnoticed.
//!
//! WHY THIS IS ONE TEST IN ITS OWN FILE: the counters are process-global
//! statics and `libtest` runs a binary's tests concurrently on many threads, so
//! a second test here could inflate another's before/after delta. Each file
//! under `tests/` is its own binary, so exactly ONE `#[test]` means nothing
//! else in the process touches these counters while it runs. Same rule as
//! `tests/item_gauges.rs` and `tests/item_dead_gauges.rs`.

#![cfg(all(feature = "metrics", not(model_checking)))]

use segcache::{Policy, Segcache};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const TOTAL_SEGMENTS: usize = 24;

/// A coarse-clocked duration is always a whole number of seconds.
const ONE_SECOND_NANOS: u64 = 1_000_000_000;

/// Read a registered counter by its exported metric name.
fn counter(name: &str) -> u64 {
    for metric in metriken::metrics().iter() {
        if metric.name() == name {
            return match metric.value() {
                Some(metriken::Value::Counter(v)) => v,
                other => panic!("metric {name} is not a counter: {:?}", other.is_some()),
            };
        }
    }
    panic!("metric {name} is not registered");
}

fn key(i: usize) -> String {
    format!("k{i:06}")
}

fn val(i: usize) -> String {
    format!("v{i:06}")
}

fn item_size() -> usize {
    keyvalue::item_size(key(0).len(), &keyvalue::Value::Bytes(val(0).as_bytes()), 0)
}

fn segment_size() -> i32 {
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    (magic_overhead + item_size() * ITEMS_PER_SEGMENT) as i32
}

fn cache_with(policy: Policy) -> Segcache {
    Segcache::builder()
        .segment_size(segment_size())
        .heap_size(segment_size() as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        .eviction(policy)
        .build()
        .expect("failed to create cache")
}

/// Assert a timing counter's delta is a real sub-second measurement.
fn assert_real_measurement(name: &str, before: u64, after: u64) {
    let delta = after - before;
    assert!(
        delta > 0,
        "{name} did not move: the operation ran but recorded no time. A coarse \
         (1-second) clock reports exactly this whenever the operation fits \
         inside one of its ticks, which is the common case."
    );
    assert!(
        delta < ONE_SECOND_NANOS,
        "{name} recorded {delta} ns for an operation that takes single-digit \
         milliseconds. A coarse (1-second) clock reports exactly {ONE_SECOND_NANOS} ns \
         per tick-straddling operation, so a whole-second reading here means \
         the measurement is back on the coarse clock."
    );
}

#[test]
fn timing_metrics_measure_real_elapsed_time() {
    let ttl = Duration::from_secs(3600);

    // ── clear_time ──────────────────────────────────────────────────────
    //
    // A roomy FIFO cache filled to a few segments, then swept once. This is
    // the assertion the pelikan side wanted for a `flush_all` regression test
    // and could not have while the counter read the same whether the sweep ran
    // once, eight times, or never.
    {
        let cache = cache_with(Policy::Fifo);
        for i in 0..(ITEMS_PER_SEGMENT * 8) {
            cache
                .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
                .expect("fill insert");
        }

        let before = counter("clear_time");
        let cleared = cache.clear();
        let after = counter("clear_time");

        assert!(cleared > 0, "the sweep must have cleared some segments");
        assert_real_measurement("clear_time", before, after);
    }

    // ── evict_time ──────────────────────────────────────────────────────
    //
    // Insert well past the heap so eviction runs many times. Nothing can
    // expire (hour-long TTL), so `evict` never takes its cheap expired-segment
    // early return and every call is timed.
    {
        let cache = cache_with(Policy::Fifo);
        let before = counter("evict_time");
        for i in 0..(ITEMS_PER_SEGMENT * TOTAL_SEGMENTS * 4) {
            let _ = cache.insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl);
        }
        let after = counter("evict_time");

        assert!(
            counter("segment_evict") > 0,
            "the fill must have driven at least one eviction"
        );
        assert_real_measurement("evict_time", before, after);
    }

    // ── expire_time ─────────────────────────────────────────────────────
    //
    // `TtlBuckets::expire` debounces itself to one pass per coarse tick, and
    // `last_expired` starts at construction time, so the pass only runs once
    // the coarse clock has advanced past the tick the cache was built in.
    // Sleeping past a tick boundary makes this deterministic rather than
    // dependent on where in the second the test started.
    {
        let cache = cache_with(Policy::Fifo);
        for i in 0..(ITEMS_PER_SEGMENT * 8) {
            cache
                .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
                .expect("fill insert");
        }

        std::thread::sleep(Duration::from_millis(1_100));

        let before = counter("expire_time");
        cache.expire();
        let after = counter("expire_time");

        assert_real_measurement("expire_time", before, after);
    }
}
