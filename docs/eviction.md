# Segcache Eviction Strategies

Eight policies on one `Policy` enum (`crates/segcache/src/eviction/policy.rs`),
all operating at segment granularity over the same TTL-bucketed storage. Five
select a whole segment and drop it; two scan items and copy the valuable ones
forward; one refuses to evict at all.

## The policy roster

| Policy | Selects | Item-level work |
|---|---|---|
| `None` | nothing — inserts fail when the heap is full | — |
| `Random` | a uniformly random evictable segment | none — whole segment dropped |
| `RandomFifo` | a random readable segment's TTL bucket, then that bucket's head — weighting eviction toward the TTL tiers that hold the most memory | none |
| `Fifo` | the globally oldest segment, aged by `max(create_at, merge_at)` | none |
| `Cte` | the segment expiring soonest: `min(create_at + ttl)` | none |
| `Util` | the segment with the fewest live bytes (most dead space) | none |
| `Merge` | a chain of ≥3 adjacent segments in one TTL bucket | frequency-based pruning; survivors compacted into a spare |
| `S3Fifo` | oldest admission-pool segment, then oldest main-pool segment | frequency-based promotion with a ghost-queue second chance |

`Fifo`/`Cte`/`Util` keep a ranked segment list refreshed at most once per
second; `Random`/`RandomFifo` sample statelessly and never rank. Every policy
shares two invariants: `expire()` runs first, freeing whole expired segments
before any policy logic; and `can_evict` excludes the bucket's Live write tail
and any reader-pinned segment (`evictable state && ref_count == 0`).

Figure color code: **blue** = kept / promoted / selected; **orange** = pruned /
dropped; **dashed** = not evictable (write tail, Relinking spare, ghost).

## Whole-segment selection — five pickers, one substrate

![Five whole-segment eviction policies choosing from one TTL-bucketed segment
field](diagrams/eviction-policies.svg)

The same segment field, five different choices. Each TTL bucket chains segments
head (oldest) to tail (the Live write target, never evicted). The
selection-only policies differ solely in which segment they point at — the
eviction itself is identical: the chosen segment is drained whole and recycled
to the free pool.

## Merge eviction — prune, compact, recycle

![Merge eviction: spare head-insert, per-candidate claim, prune, copy
survivors, recycle](diagrams/eviction-merge.svg)

The segcache-paper policy, in six steps:

1. A spare segment is reserved and head-inserted into the TTL bucket in
   *Relinking* state — readable (so relinked survivors stay reachable) but not
   evictable, so a concurrent evictor can neither select nor claim it.
2. Each chain candidate is claimed *Sealed → Draining* before any mutation —
   the uniform per-segment claim shared by every mutator.
3. Items below a running frequency cutoff are pruned (marked deleted — no
   bytes move).
4. Survivors are appended into the spare and republished item-by-item with a
   Release-CAS on the hashtable location; live bytes of a readable segment are
   never moved in place.
5. The drained candidate is finalized: unlinked from the chain and recycled to
   the free pool (a reader-pinned candidate is condemned to its last reader
   instead).
6. The filled spare is published *Relinking → Sealed* and remains the bucket
   head.

Entry and bounds: eviction starts at a random TTL bucket's `next_to_merge`
cursor and needs a chain of at least 3 evictable segments; with no spare
available it degrades to dropping the chain head whole. A pass stops when it
has merged the maximum segment count, the spare reaches `stop_ratio`, a
candidate is unevictable, or a drain claim is lost. A compaction sub-mode
(triggered from `remove_at` when occupancy falls below `1/compact`) uses the
same copy machinery without pruning, and skips instead of dropping when no
spare is available.

## S3-FIFO — admission, ghost, second chance

![S3-FIFO: admission pool, promotion to main, ghost queue second chance, CLOCK
sweep](diagrams/eviction-s3fifo.svg)

Two segment pools plus a bounded ghost FIFO of evicted key hashes. New keys
enter the admission pool (sized by `admission_ratio`). Admission eviction runs
first and promotes touched items (freq > 0) to the main pool by copy, while
untouched items are dropped and their key hashes recorded in the ghost queue; a
later insert that hits the ghost skips admission entirely — proof of a second
request earns direct main placement, so one-hit wonders die in admission.
Main-pool eviction is a CLOCK sweep: items with freq > 0 get one more chance in
a fresh main segment, the rest are dropped. Promotion and second-chance copies
reuse the merge relink machinery: copy bytes, then Release-CAS the hashtable
location. See [s3fifo.md](s3fifo.md) for the full design.

## Provenance

The three figures are **generated — do not edit**:
`python3 docs/diagrams/eviction_diagrams.py` regenerates them in place. The
generator asserts 22 source claims plus one ordering claim (admission-pool
eviction precedes main) against `crates/segcache` and aborts on drift; all
drawn geometry is bounds-checked into each figure's viewBox. Current render
derived at commit `073cce5` with `crates/segcache` clean. Freshness is manual
for now — regenerate after eviction-code changes; a CI check that regenerates
and diffs the committed SVGs is a natural follow-up since emission is
deterministic.
