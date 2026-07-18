# Drain-Safe Merge Eviction — Design

Item 5 of the segcache concurrency roadmap, scope **(b)**: replace the only
remaining in-place-on-readable operation — `Segment::compact()`, used by the
Merge eviction policy — with a reader-safe copy-to-spare protocol. After this,
no eviction path mutates a readable segment's bytes in place, so a concurrent
reader pinning a segment can never have bytes moved under its pointer.

Follows #25 (reader pinning), #28 (segment state machine + condemn/free queue),
#29 (CAS linearization), and #30 (concurrent reserve) toward the item-7 goal of
an `Arc`-shareable `Segcache`. Like those, eviction stays `&mut`-serialized;
this is machinery built and tested behind the still-`&mut` API so item 7 can
flip reads to `&self`.

## Scope

**In scope (b):** the reader-vs-compact hazard. `merge_evict` and
`merge_compact` call `Segment::compact()`, which relocates surviving items to
the front of a `Sealed` (readable) segment in place. This is the last in-place
mutation of readable data in the eviction paths.

**Explicitly out of scope:** the writer-vs-drain hazard (a) — the reserve path
racing eviction — and the generation-less seal CAS flagged in the #30 review.
Those remain deferred; their `SCOPE(item-5)` comments are re-tagged to a
follow-up rather than removed.

**Already safe, untouched:**
- Simple policies (Random, RandomFifo, Cte, Util) drop whole segments via
  `clear_segment`/`condemn` — no in-place mutation.
- S3-FIFO (`s3fifo_evict_admission`/`_main`) already copies survivors into a
  fresh `reserve_free()` target and drains the source. This is the exact
  pattern Merge adopts; it is the in-tree template.
- `Segment::copy_into(src, dst)` already appends a source's survivors past the
  destination's write-offset (each published by the hashtable's Release-CAS
  relink). Reader-safe as-is; it is retargeted, not rewritten.

## Decisions

1. **Copy-to-spare, following the in-tree s3fifo pattern — no `Relinking`.**
   Copying survivors *out of* a `Sealed` (or drained) candidate into a fresh
   spare, then `clear_segment`-draining the candidate, is reader-safe without
   entering `Relinking`: bytes are never moved in place, and the condemn
   machinery handles pinned readers. `Relinking`/`Locked` stay declared-unused
   (a comment records that s3fifo/merge made them unnecessary under serialized
   eviction).
2. **Spare reservation, default 1, Merge-policy only** (crucible's
   `reserve_spare` model). Copy-to-spare needs a destination, but eviction runs
   when the pool is full, so a normal `reserve_free()` returns `None`. A small
   held-back spare guarantees Merge always has a destination.
3. **Expire-first at the `evict()` entry.** Cheap whole-segment expiration is
   attempted before the spare-consuming merge; if it frees a segment, merge is
   skipped entirely.

## 1. Merge rework (`merge_evict`, `merge_compact`)

**Today:** the first chain segment is the destination; it is pruned and
`compact()`-ed in place (the hazard), then sources are `copy_into`-ed and
recycled.

**New**, mirroring `s3fifo_evict_main` with N candidates instead of 1:

1. Acquire a spare via `reserve_spare()` (§2). If `None`, degrade gracefully to
   dropping the head candidate whole via `clear_segment` (like s3fifo), rather
   than corrupting the merge.
2. Head-insert the spare into the target TTL bucket as `Sealed`
   (`link_at_head` + `bucket.set_head`). Set its ttl, pool, `bucket_id`, and
   `mark_merged()` timestamp.
3. For each candidate, using the existing adaptive prune cutoff:
   - `prune(candidate)` (marks low-frequency items deleted; does not move
     bytes — reader-safe).
   - `copy_into(candidate, spare)` — append survivors past the spare's
     write-offset; each survivor is relinked by the hashtable's existing
     Release-CAS (`cas_location`), which orders the copied bytes ahead of the
     new location becoming visible.
   - `clear_segment(candidate)` — drain it (`Sealed→Draining` + `ref_count`
     recheck + condemn-if-pinned). A candidate pinned by a reader is condemned
     to `AwaitingRelease`; the reader keeps reading valid bytes and the last
     guard drop frees it.
   - Stop when the spare reaches `stop_bytes`, or when a candidate cannot be
     drained (pinned and the pass chooses to stop), preserving the current
     stop conditions.
4. **`Segment::compact()` is deleted** — both call sites are gone. `copy_into`
   stays, retargeted from the first candidate to the spare.

**Reader-safety invariant:** no readable segment's live bytes are ever moved in
place. Survivors are appended to a fresh spare and published via the hashtable
Release-CAS; a pinned reader on a candidate reads intact bytes until
`clear_segment` condemns it and its guard drops (#25/#28 machinery, unchanged).

## 2. Spare reservation (`Segments`)

New fields:
- `spare_queue: Box<crossbeam_deque::Injector<u32>>`
- `spare_capacity: u32`
- `spare_count: AtomicU32` (atomic for item-7 readiness, matching the lock-free
  free queue built in #28 even under `&mut`)

Behavior:
- **`spare_capacity`** is set at construction: `1` for `Policy::Merge`, `0`
  otherwise (s3fifo degrades gracefully; simple policies drop whole segments;
  neither compacts). At construction, `spare_capacity` segments seed the
  `spare_queue`, the rest seed the `free_queue`.
- **`reserve_free()` (normal writes): unchanged** — pulls from `free_queue`
  only, never the spare. Load-bearing: normal allocation cannot drain the
  reserve, so a spare is always available to Merge at "full."
- **`reserve_spare()` (merge only):** steal from `spare_queue` → `try_reserve`;
  if empty, fall back to `reserve_free`. Returns a `Reserved` segment.
- **`return_segment(id)`** — a shared helper both `recycle` and `condemn`'s
  free-path call: if `spare_count < spare_capacity`, push to `spare_queue` and
  bump `spare_count`; else push to `free_queue`. Replenishes the spare before
  the general pool, restoring the reserve after each merge.
- **Metrics:** `SEGMENT_FREE` counts free+spare (segments available);
  `reserve_spare` from the spare decrements, replenishment increments — same
  accounting shape as today. `free()`/`items()` test helpers count both queues.

Net capacity: a Merge-policy cache holds back exactly 1 segment.

## 3. Expire-first at `evict()`

At the `evict()` entry, attempt `ttl_buckets.expire()` first (crucible's
`try_expire_segments`). If it frees ≥1 segment, return success immediately — no
`reserve_spare`, no copy, no merge. Expiration drops whole segments through the
same `clear_segment`/condemn path (reader-safe, no in-place mutation), so it is
consistent with scope (b) and keeps the held-back spare untouched whenever
expired segments can simply be dropped.

## 4. Testing

- **Existing suite passes unchanged** — `integration_eviction.rs`, merge and
  s3fifo unit tests. Which items survive a merge and which segments free is
  preserved; only the destination changed from in-place to spare.
- **Reader-safety stress test** (crate-internal, the new teeth): N reader
  threads pin items in a bucket via `acquire_item_at` while an evictor thread
  runs merge eviction on that bucket. Assert every pinned reader reads intact
  key/value bytes for its guard's lifetime, no segment is freed while pinned
  (ref_count gating), and no leak (free + spare + chain == total). Bite-check:
  temporarily restoring in-place `compact()` must make this test fail.
- **Spare-queue unit tests:** construction split (1 spare + rest free for
  Merge; 0 spare otherwise); `reserve_spare` falls back to `free_queue` when
  the spare is empty; `return_segment` replenishes the spare before the free
  queue; a full-pool merge succeeds because the spare is available.
- **Loom:** none added. Eviction stays `&mut`-serialized (no election to
  model); the reader-vs-drain safety is the #25/#28 SeqCst Dekker pair, already
  loom-scoped and not loom-verifiable. Revisit only if the reader-safety stress
  test surfaces a race worth modeling.

## 5. Non-goals

- Writer-vs-drain protocol (hazard a) and the generation-less seal CAS —
  deferred; `SCOPE(item-5)` comments at `drain_chain` and `try_alloc_item` are
  re-tagged to a follow-up, not removed.
- `Relinking`/`Locked` states — remain declared-unused, with a comment noting
  the copy-to-drain pattern made them unnecessary under serialized eviction.
- Reads and the public API stay `&mut self`; this is machinery for item 7.
