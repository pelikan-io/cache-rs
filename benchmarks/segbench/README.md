# segbench

A multi-threaded scaling harness for `segcache`: N OS threads sharing one
`Arc<Segcache>` and calling `get`/`insert` directly. No sockets, no protocol
parsing — the numbers are the engine's.

`cargo bench -p segcache` answers "how fast is one operation?" with
single-threaded criterion microbenchmarks. This answers a different question:
**how does throughput change as threads are added?** Those diverge sharply on
the write path, and nothing in the repo measured the second one.

## Running

```bash
cargo build --release -p segbench

# one measurement
./target/release/segbench <threads> <write_pct> <dist> <warmup_s> <measure_s> [mode] [args...]

# a whole sweep
CPUS=8-15 ./benchmarks/segbench/sweep_scaling.sh
CPUS=8-15 ./benchmarks/segbench/sweep_locality.sh
python3 benchmarks/segbench/aggregate.py benchmarks/segbench/results/*.csv
```

Each run prints one CSV row: `threads,write_pct,dist,mode,mops`. Sweep scripts
take `CPUS`, `THREADS`, `BIN` and `OUT` from the environment.

### Pin to one kind of core

`CPUS` is passed to `taskset`. Use it. On a hybrid CPU an unpinned sweep spreads
threads over performance cores, their SMT siblings, and efficiency cores, then
plots all three against a single "threads" axis — the resulting curve says more
about the core mix than about the engine. On the i5-13500H the results here were
taken on, `CPUS=8-15` selects the eight E cores: no SMT, one thread per core,
so a thread count is a core count.

Threads are confined to that CPU set, not pinned one-to-one within it. With as
many threads as cores on an otherwise idle set the difference is inside the
run-to-run spread.

## Workload

Fixed at 1M keys, 16 B keys, 128 B values, TTL 0, prefilled before measurement.
Per op each thread draws a key (uniform or Zipf s=0.99) and rolls `write_pct`
for GET vs SET-replace. Writes are same-size replaces, so segment reclamation
runs during measurement rather than only at the end.

Threads count their own ops and add them to a shared total when a measurement
flag flips, sampled every 64 ops to keep the check off the hot path.

## Modes

| Mode | What it measures |
|---|---|
| `base` (default) | One shared engine, TTL 0 — a single TTL bucket, so all writers share one active tail. |
| `shuf` | `base` with prefill in shuffled key order. Spreads a Zipf head across segments instead of packing it into the segments filled first, which separates per-segment contention from key contention. |
| `stripe` | Shared engine, thread *t* writes with TTL `1000 + 8t` s, landing in its own tier-1 TTL bucket: per-thread tails through the public API. Each stripe also gets its own merge chain, so it is an upper bound for a tail-only change. |
| `shard <>` | T private single-writer engines, heap `1024/T` MB each, keys partitioned `idx % T`, each thread driving only its own shard. The no-routing upper bound for sharded designs. |
| `part <P>` | P engines behind one routing function, every thread reading and writing every partition. Isolates the partitioned data layout from the ownership discipline. |
| `delegate <batch> [owners] [ns]` | Workers route ops by key hash over bounded channels to owner threads holding private shards. `batch` = requests per message; `ns` = calibrated per-op worker busy-work modelling parse/socket cost. |
| `hybrid <batch> [owners] [ns]` | Shared engine; workers execute reads in place, writes are batch-routed to owners applying with per-owner TTL stripes. |

TTL 0 is deliberate: it concentrates every writer on one TTL bucket, which is
both realistic for a single-default-TTL deployment and the worst case for tail
reservation contention.

## Reading the output

Compare a series against **its own** single-thread median, not against another
series' absolute Mops/s — the distributions have different per-op sampling costs
that have nothing to do with the engine. `aggregate.py` computes speedups that
way.

Findings from the sweeps committed under `results/` are written up in
[`docs/journal/2026-08-25-segcache-read-write-scaling.md`](../../docs/journal/2026-08-25-segcache-read-write-scaling.md).
