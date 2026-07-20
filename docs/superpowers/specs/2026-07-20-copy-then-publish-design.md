# Copy-Then-Publish in Eviction Copy Paths — Design

Item **7a** of the segcache concurrency roadmap (the first slice of item 7, the
`&self`/Arc-shareable API). Closes the deferred prerequisite from the item-5b
review: the eviction copy paths publish the hashtable location *before* writing
the copied bytes — a torn-read hole that opens once reads become `&self`.

This PR fixes the ordering and verifies it with a loom model. The public API
stays `&mut self` (the reads flip is item 7b); nothing here is reachable as a
bug today, but the reorder is a prerequisite for safe concurrent reads.

## Background

Two eviction copy paths relocate a live item into a destination segment and
relink the hashtable to the new location:

- `Segment::copy_into` (segment.rs) — used by `merge_evict` / `merge_compact`.
- `s3fifo_promote_from` (segments.rs) — used by S3-FIFO eviction.

Both currently do **publish-then-copy**:

```rust
if cas_location(old, new) {         // publish the new location FIRST
    copy_nonoverlapping(src, dst);  // ...then write the bytes  ← torn-read window
    remove_item_at; incr counters; advance write_offset;
}
```

Under the current `&mut`-serialized API this is safe — no reader runs
concurrently. But once reads are `&self` (item 7b+), a reader can Acquire-load
the freshly published location and read `dst[write_offset]` **before**
`copy_nonoverlapping` has run — a torn/garbage read. This is exactly the hazard
the item-5b review flagged and deferred.

`replace_at` (segcache.rs) is NOT affected: it publishes a `ReservedItem` whose
bytes were already written by `define()` — it is already copy-then-publish.

## Decision

Reorder both copy sites to **copy-then-publish**:

```rust
copy_nonoverlapping(src, dst);      // write bytes into dst[write_offset] first
if cas_location(old, new) {         // Release-CAS publishes; orders bytes ahead of it
    remove_item_at; incr counters; advance write_offset;
}
// cas failed: bytes sit orphaned at dst[write_offset]; write_offset is NOT
// advanced. copy_into aborts the merge with Err (the bytes persist until the
// destination is later reset/recycled); s3fifo_promote_from continues, and the
// next copy reuses the same offset. Either way there is no reader hazard —
// nothing points at dst[write_offset] until a successful publish, and readers
// reach an item only via its hashtable location.
```

`cas_location`'s success ordering is already `Release`, which orders the
preceding byte copy ahead of the publish. No extra fence is needed. Single-
threaded behavior is identical — the reorder is observable only under
concurrency — so all existing merge / S3-FIFO tests pass unchanged.

## Reader-safety argument

A reader reaches a relocated item only by resolving its key in the hashtable to
`new` and then reading `dst` at the encoded offset. With copy-then-publish:

1. Writer writes the bytes into `dst[write_offset]` (Relaxed is sufficient for
   the bytes themselves; the ordering comes from step 2).
2. Writer `cas_location(old→new)` with `Release` success ordering — this
   establishes a release edge after the byte writes.
3. Reader Acquire-loads the slot, observes `new`, and reads `dst` — the Acquire
   pairs with the writer's Release, so the byte writes happen-before the read.

The CAS-failure path writes bytes that are never published (write_offset not
advanced, no hashtable entry points at them), so they are invisible to readers
and overwritten by the next copy.

## Testing

- **Existing suite passes unchanged** — merge, S3-FIFO, and all eviction tests.
  The reorder is behaviorally identical single-threaded.
- **Loom model (the teeth).** copy-then-publish is a message-passing pattern
  (Release-publish → Acquire-observe → read payload), which is SC-independent
  and within loom's power — unlike the reader-pinning SeqCst Dekker pairs, which
  loom cannot verify. Model, using the *real* `cas_location` on a loom-
  instrumented `MultiChoiceHashtable` slot plus a stand-in payload atomic for a
  segment byte:
  - Writer thread: store a sentinel into the payload (Relaxed), then
    `cas_location(old→new)` (real Release-CAS).
  - Reader thread: if the slot resolves to `new` (Acquire), load the payload —
    it must be the sentinel, never the initial value.
  - Assertion: no interleaving observes the published location with a stale
    payload. SC-independent (CAS + message-passing), so no false loom violation.
  - Bite-check: reverting the writer to publish-then-store makes loom find the
    violation — confirming the model has teeth for the ordering.
  Test name contains "loom" for the CI filter (`--features loom -- loom`).

## Non-goals / deferred

- **End-to-end concurrent racing-pin stress** (a real reader reading a
  `copy_into`'d item across a live merge) needs a shareable cache — `&self`
  reads plus internally-synchronized eviction — which lands in later item-7
  slices. This PR delivers the ordering fix + its loom verification; the
  concurrent stress test is item 7e. The PR will state this plainly rather than
  over-claim (the honesty lesson from item 5b's reader-safety test).
- Public API stays `&mut self`. No reads flip here (item 7b).

## Cleanup

- Update the item-5b spec §1 note and the roadmap-memory entry that flagged this
  as an item-7 prerequisite to record it as done in 7a.
