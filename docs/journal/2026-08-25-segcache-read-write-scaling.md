---
status: shipped
opened: 2026-08-25
updated: 2026-08-25
---

# Read and write scaling of the shared segcache engine

## Goal

Measure how throughput of one shared `Arc<Segcache>` changes as threads are
added, separately for reads and for writes, and land the harness that produced
the numbers so a later fix can be checked against them.

The engine became concurrent in 0.4.x — `get` takes `&self` — and pelikan PR
#189 now runs N workers against one instance in place. Nothing in this
repository measured that. `cargo bench -p segcache` is single-threaded criterion
microbenchmarks: it answers how fast one operation is, not what happens to
aggregate throughput at eight threads.

A prior experiment lived in pelikan under
`docs/journal/2026-08-19-segcache-write-scaling/`. It swept 1 to 16 threads on a
hybrid CPU without pinning, so its thread axis mixed performance cores, their
SMT siblings, and efficiency cores. It also measured only a 50/50 mix, which
turned out to hide the result.

## Decision Criteria

- Read and write scaling reported separately, each measured directly rather
  than inferred from a mixed workload.
- Thread count equals core count: one core type, no SMT.
- Committed results reproducible from the committed code — same engine version,
  same dependency versions, same scripts.

## Scope

New workspace member `benchmarks/segbench` (harness, sweep scripts, aggregator,
raw CSVs) and this entry. `crossbeam-channel` and `rand_distr` added to
`[workspace.dependencies]`; `segcache` added as a workspace path dependency. No
existing crate touched, and no engine behavior changed.

## Evidence

Host: i5-13500H, `CPUS=8-15` — the eight Gracemont E cores, no SMT, 3.5 GHz, two
4-core clusters with 2 MB L2 each. 32 GB, Linux 6.8, rustc 1.97.1, `--release`.
Cache: 1 GiB heap, 1 MiB segments, hash_power 22, Merge eviction. Workload: 1M
keys, 16 B keys, 128 B values, TTL 0, prefilled. 2 s warmup + 8 s measured, 3
interleaved repeats, median. Run-to-run spread is under 2% at every point.

```
CPUS=8-15 ./benchmarks/segbench/sweep_scaling.sh
CPUS=8-15 ./benchmarks/segbench/sweep_locality.sh
python3 benchmarks/segbench/aggregate.py benchmarks/segbench/results/*.csv
```

Median Mops/s, with speedup against each series' own single-thread median:

| Cores | read uniform | read zipf | write uniform | write zipf | 50/50 uniform |
|---|---|---|---|---|---|
| 1 | 2.662 (1.00x) | 1.924 (1.00x) | 0.982 (1.00x) | 0.953 (1.00x) | 1.440 (1.00x) |
| 2 | 5.509 (2.07x) | 3.681 (1.91x) | 0.908 (0.92x) | 1.012 (1.06x) | 1.679 (1.17x) |
| 4 | 11.016 (4.14x) | 6.880 (3.58x) | 0.975 (0.99x) | 1.073 (1.13x) | 1.824 (1.27x) |
| 6 | 16.483 (6.19x) | 9.274 (4.82x) | 0.952 (0.97x) | 1.056 (1.11x) | 1.844 (1.28x) |
| 8 | 21.796 (8.19x) | 11.371 (5.91x) | 0.808 (0.82x) | 0.910 (0.95x) | 1.639 (1.14x) |

Verification: `cargo clippy -p segbench --all-targets --all-features -- -D
warnings` and `cargo fmt --all --check` both clean;
`cargo build --release -p segbench` clean.

## Design and Implementation

The harness shares one `Arc<Segcache>` across N OS threads calling `get` and
`insert` directly. Threads count their own operations and add them to a shared
total when a measurement flag flips, sampled every 64 operations to keep the
check off the hot path. Writes are same-size replaces, so segment reclamation
runs during measurement instead of only at the end.

Three changes separate this from the pelikan original:

**Pin to one core type.** `CPUS` is passed to `taskset`. On a hybrid CPU an
unpinned sweep plots three kinds of resource against one "threads" axis, and the
resulting curve describes the core mix more than the engine.

**Measure the endpoints, not the mixture.** Sweeping 0% and 100% writes as well
as 50/50 is what exposed the write result. Decomposing a mixed run as read and
write capacity in series, `0.5/R + 0.5/W = 1/M`, recovers a write capacity of
0.99 / 0.99 / 0.99 / 0.98 / 0.85 Mops/s at 1/2/4/6/8 cores, within 5.4% of the
direct measurement at four of five points (2 cores is the loose one, +9.1%,
where the direct write run dips and the mixture does not). Two routes, one
answer, neither assuming a mechanism.

**A control for segment locality.** `mode=shuf` prefills in shuffled key order.
Everything else is identical, so it moves a Zipf head off the few segments that
index-ordered prefill fills first and spreads it over many.

## Outcome

**Reads scale linearly; writes do not scale at all.** On eight equal cores,
uniform reads return 8.19x and uniform writes 0.82x. The best write speedup
anywhere in the sweep is 0.99x (uniform, 4 cores) and 1.13x (Zipf, 4 cores). The
write path behaves as a single serial resource reachable from any core, and the
eighth core makes it worse: 0.952 to 0.808 Mops/s between 6 and 8 cores.

**The 50/50 mixture hides this.** Its 1.28x peak at 6 cores is the read half
scaling while the write half stays near 1 Mops/s. The pelikan experiment
reported that shape as write scaling; measured on its own, the write path never
rises.

**Zipf is no worse than uniform for writes — it is slightly better** (1.073
against 0.975 at 4 cores). Hot keys do not worsen write contention, which places
the bottleneck in structures every writer touches regardless of key: the shared
tail reservation, the free-segment supply, and the merge-eviction pipeline.

**Skewed reads have a separate ceiling, and it is segment locality.** Zipf reads
reach 5.91x against uniform's 8.19x. Shuffling the prefill order lifts them to
14.749 Mops/s and 7.81x, a 30% gain, without changing the workload. Both null
controls hold: uniform reads move 0.5% under the same shuffle, and Zipf writes
0.0%. Only skewed reads are sensitive to which segments the hot keys occupy.

The mechanism is in the read path. `Segcache::get` pins the item's segment
before reading it: `SegmentHeader::try_acquire_reader`
(`crates/segcache/src/segments/header.rs`) does a SeqCst `fetch_add` on that
segment's `ref_count`, re-checks the state, and the guard drop decrements it. A
read is a full-barrier read-modify-write on a line shared by every reader of
that segment. With 1 MiB segments holding about 7,280 consecutive keys, an
index-ordered prefill puts most of a Zipf head on one such line.

Index-ordered prefill is the worst case and a shuffle is close to the best; a
production key layout sits between the two curves. The gap measures the hazard
rather than predicting it.

## Derived Documents

`benchmarks/segbench/README.md` — how to run the harness, what each mode
measures, and why pinning to one core type is required.

## Deferred or Reopen Items

None of these were attempted here; the entry records measurements only.

- Stripe the active tail per TTL bucket. The pelikan experiment measured an
  emulation of this through the public API and saw negative scaling become flat.
- Parallelize segment reclaim — per-stripe free lists, concurrent merge drains.
  The residual write ceiling lives here once tail contention is removed.
- Reduce per-segment reader pin traffic. `get_no_freq_incr` already avoids the
  frequency CAS but not the pin, so the pin is the part needing striping, an
  epoch scheme, or hazard pointers. Re-run `sweep_locality.sh`: the fix should
  close the gap between the two Zipf read curves.
- Sweep segment sizes. Pin contention scales with how many hot keys share a
  segment, so segment size is a parameter of the effect and was not varied.
- `CLAUDE.md` states that reads require `&mut self` and that workloads partition
  across threads with each owning an instance. That predates the concurrent
  engine and contradicts `get(&self)`.

## Appendix: Skills Invoked

- `engineering-journal` — this entry and the index row.
- `technical-prose` — word-level pass over the entry.
- `artifact-design`, `dataviz` — the published chart set of these results.
