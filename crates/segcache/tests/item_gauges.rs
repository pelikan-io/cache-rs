//! Global item-gauge conservation.
//!
//! `ITEM_CURRENT` / `ITEM_CURRENT_BYTES` are process-global `metriken`
//! gauges maintained in lockstep with the per-segment header counters:
//! `Segments::try_alloc_item` increments them, `Segment::remove_item_at`
//! decrements them, relocation sites (`Segment::copy_into`,
//! `Segments::s3fifo_promote_from`) must be gauge-NEUTRAL because a
//! relocation MOVES an item rather than killing one, and residue left by
//! unpinned unlinks is reconciled by `SegmentHeader::reset_write_stats`
//! when the segment recycles. If any of those sites is wrong, the error is
//! permanent and cumulative — so after a storm that drains every segment,
//! the gauges must be back at exactly zero.
//!
//! WHY THIS LIVES IN `tests/` AND IS THE ONLY TEST IN THE FILE: the gauges
//! are process-global statics, and `libtest` runs the tests of one binary
//! concurrently on many threads. Any absolute (or even before/after delta)
//! assertion made from a unit test inside `src/` would race every other
//! test that inserts or evicts an item, making it nondeterministically
//! flaky. Each file under `tests/` is compiled into its OWN test binary
//! (its own process), so keeping exactly ONE `#[test]` here means nothing
//! else in the process can touch the gauges while it runs. Do not add a
//! second test to this file — add another file instead.

#![cfg(all(feature = "metrics", not(feature = "loom")))]

use segcache::{Policy, Segcache};
use std::time::Duration;

const ITEMS_PER_SEGMENT: usize = 8;
const TOTAL_SEGMENTS: usize = 24;

/// Read a registered gauge by its exported metric name.
fn gauge(name: &str) -> i64 {
    for metric in metriken::metrics().iter() {
        if metric.name() == name {
            return match metric.value() {
                Some(metriken::Value::Gauge(v)) => v,
                other => panic!("metric {name} is not a gauge: {:?}", other.is_some()),
            };
        }
    }
    panic!("metric {name} is not registered");
}

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

/// After a workload that drives S3-FIFO promotions, unpinned unlinks and a
/// full drain of every segment, the global item gauges must return to zero.
///
/// The S3-FIFO policy is deliberate: its `s3fifo_promote_from` relocation
/// used to `remove_item_at` the source (gauge -1) and bump only the
/// destination's HEADER counters, never re-adding to the gauges — so every
/// promoted item leaked a permanent -1 (and -item_size) and the gauges
/// ended the storm NEGATIVE rather than zero.
#[test]
fn item_gauges_return_to_zero_after_drain_storm() {
    // Sized so a segment holds exactly ITEMS_PER_SEGMENT items.
    let item_size =
        keyvalue::item_size(key(0).len(), &keyvalue::Value::Bytes(val(0).as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Nothing else runs in this process, so the gauges start clean.
    assert_eq!(gauge("item_current"), 0, "gauges must start at zero");
    assert_eq!(gauge("item_current_bytes"), 0, "gauges must start at zero");

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * TOTAL_SEGMENTS)
        .hash_power(16)
        // admission_ratio 0.25 -> 6 of the 24 segments form the admission
        // pool; evicting an admission segment is what promotes hot items.
        .eviction(Policy::S3Fifo {
            admission_ratio: 0.25,
        })
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);

    // Phase 1: fill roughly the admission pool.
    let admission_fill = 6 * ITEMS_PER_SEGMENT;
    for i in 0..admission_fill {
        cache
            .insert(key(i).as_bytes(), val(i).as_bytes(), None, ttl)
            .expect("admission-pool fill insert");
    }

    // Phase 2: heat a subset so their frequency counters are non-zero.
    // Zero-frequency items are dropped on admission eviction; only these
    // take the promotion path.
    let hot = 3 * ITEMS_PER_SEGMENT;
    for _ in 0..5 {
        for i in 0..hot {
            let _ = cache.get(key(i).as_bytes());
        }
    }

    // Phase 3: drive the cache well past capacity so admission segments are
    // evicted repeatedly (promoting the hot set), and mix in deletes and
    // same-key overwrites so unpinned unlinks and reservation rollbacks
    // occur alongside the relocations.
    let churn = TOTAL_SEGMENTS * ITEMS_PER_SEGMENT * 6;
    for i in 0..churn {
        let k = 1_000 + i;
        let _ = cache.insert(key(k).as_bytes(), val(k).as_bytes(), None, ttl);
        // Keep re-heating the hot set so it keeps promoting.
        if i % 4 == 0 {
            let _ = cache.get(key(i % hot).as_bytes());
        }
        // Overwrite (replace arm) and delete, both of which can unlink
        // without a remover pin when they race the eviction drains.
        if i % 3 == 0 {
            let _ = cache.insert(key(k).as_bytes(), val(k + 1).as_bytes(), None, ttl);
        }
        if i % 5 == 0 {
            let _ = cache.delete(key(k).as_bytes());
        }
    }

    // Phase 4: the same churn CONCURRENTLY with a drainer, which is the only
    // way to produce unpinned unlinks (a delete/replace losing its remover
    // pin to a drain, a reservation rollback into a draining segment). Those
    // skip `remove_item_at` entirely and leave a residue that only
    // `reset_write_stats` reconciles — the path the ITEM_DEAD mirroring
    // below exists for. Bounded work, and every thread is joined before the
    // gauges are read, so the final state is still deterministic.
    let cache = std::sync::Arc::new(cache);
    std::thread::scope(|scope| {
        for t in 0..3 {
            let cache = std::sync::Arc::clone(&cache);
            scope.spawn(move || {
                for i in 0..2_000 {
                    let k = 10_000 + t * 100_000 + i;
                    let _ = cache.insert(key(k).as_bytes(), val(k).as_bytes(), None, ttl);
                    let _ = cache.insert(key(k).as_bytes(), val(k + 1).as_bytes(), None, ttl);
                    let _ = cache.get(key(k).as_bytes());
                    if i % 2 == 0 {
                        let _ = cache.delete(key(k).as_bytes());
                    }
                }
            });
        }
        let cache = std::sync::Arc::clone(&cache);
        scope.spawn(move || {
            for _ in 0..200 {
                let _ = cache.clear();
            }
        });
    });

    // Phase 5: quiesce. Drain every segment from the hashtable; repeat until
    // a pass frees nothing, so every segment that can recycle has recycled
    // (and run `reset_write_stats`).
    for _ in 0..16 {
        if cache.clear() == 0 {
            break;
        }
    }
    assert_eq!(cache.clear(), 0, "cache must be fully drained");
    assert!(
        cache.get(key(0).as_bytes()).is_none(),
        "nothing may survive the drain"
    );

    // KNOWN GAP (theoretical, not observed here): a segment condemned while
    // still reader-pinned is freed by the last `SegmentGuard::drop`, which
    // pushes it straight onto the free queue without passing through
    // `recycle` or `try_reserve` — so its unpinned-unlink residue is not
    // reconciled by `reset_write_stats` until it is re-reserved. This test
    // holds no `Item`s across a drain, so nothing is ever condemned and the
    // total lands exactly on zero; a workload that did could leave a
    // transient residue here. Follow-up tracked separately.
    assert_eq!(
        gauge("item_current"),
        0,
        "ITEM_CURRENT did not return to zero after every segment drained \
         (negative => a relocation site decremented without compensating; \
         positive => an item was allocated without ever being accounted dead)"
    );
    assert_eq!(
        gauge("item_current_bytes"),
        0,
        "ITEM_CURRENT_BYTES did not return to zero after every segment drained"
    );

    // Every item that left `ITEM_CURRENT` must have been mirrored into
    // `ITEM_DEAD` — on the normal path by `Segment::remove_item_at`, and for
    // items unlinked WITHOUT a remover pin by the residue reconciliation in
    // `SegmentHeader::reset_write_stats`. `ITEM_CURRENT` was raised
    // `item_allocate` times by fresh allocations and `item_relink` times by
    // relocation compensation, and it is now back at zero, so exactly that
    // many items must have been accounted dead.
    //
    // NOTE: this identity encodes the CURRENT cumulative semantics of
    // `item_dead` — in particular that a relocation counts as a death (each
    // relink runs `remove_item_at`, which adds to `ITEM_DEAD`, and is offset
    // on the live side by the `item_relink` term). If the follow-up that
    // reconsiders those semantics lands — e.g. relocations stop counting as
    // deaths, or the metric becomes a true gauge with decrements — this
    // assertion has to be revisited, not merely re-baselined.
    assert_eq!(
        gauge("item_dead"),
        (counter("item_allocate") + counter("item_relink")) as i64,
        "ITEM_DEAD does not account every item that left ITEM_CURRENT"
    );
}
