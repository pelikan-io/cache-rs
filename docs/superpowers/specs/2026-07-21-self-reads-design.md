# `&self` Reads — Design

Item **7b** of the segcache concurrency roadmap (second slice of item 7, the
`&self`/`Arc`-shareable API). Flips the read methods from `&mut self` to
`&self`, establishes `Segcache: Sync`, and adds a concurrent-reader stress test
— the first time the read path runs truly concurrently.

Follows 7a (#32, copy-then-publish). Writes and eviction stay `&mut`
(exclusive), so reader-vs-writer concurrency is still impossible — that arrives
in 7c/7d. This is a real, tested milestone: concurrent readers on a shared
cache.

## Background

The read path is already `&self`-ready. `Segcache::get`'s body uses only `&self`
operations: `verifier()` (`&self`), `hashtable.lookup` (`&self` — the crucible
atomic-slot port), `acquire_item_at` (`&self`, the reader-pinning from #25),
`generation` (`&self`). Only the `&mut self` receiver forces exclusivity.

`MultiChoiceHashtable` already carries `unsafe impl Send + Sync` (table.rs:35-36)
— it vouches for its own raw-pointer internals. Every other field of `Segcache`
/ `Segments` is an ordinary `Sync` type (anonymous mmap `MmapMut`, atomic
headers, lock-free `Injector` queues, `Xoshiro256PlusPlus` RNG, atomic TTL-bucket
links), so `Segcache` should **auto-derive `Sync`**.

## Decisions

1. **Flip the read receivers to `&self`.** Production: `get`, `get_no_freq_incr`
   (pure receiver change — bodies are already `&self`). Debug-gated: `items()`
   (needs a `&self` `Segments::items()` that sums atomic `live_items`) and
   `check_integrity()` (its `Segments` side is already `&self`).
2. **Establish `Segcache: Sync` via a compile-time guard, not a fresh
   `unsafe impl`.** Add:
   ```rust
   const _: () = {
       fn assert_sync<T: Sync>() {}
       let _ = assert_sync::<Segcache>;
   };
   ```
   If it compiles (expected), rely on auto-`Sync`; the guard locks it so a future
   `!Sync` field breaks the build loudly. Only if a specific field blocks it do
   we add a *justified* `unsafe impl Sync` following the hashtable precedent.
   `Send` is NOT added here — 7b needs only `Sync` (`&Segcache: Send` across
   scoped threads); `Send` for `Arc` is 7e.

## Soundness

Concurrent `&self` reads are safe because every read touches only atomic /
lock-free state: the hashtable frequency update is an atomic CAS, the reader pin
is an atomic `ref_count` RMW, and item bytes are read-shared (never mutated by a
read). The still-`&mut` write / eviction methods require *exclusive* access, so
the borrow checker forbids a write from overlapping shared reads. Reader-vs-
writer concurrency — and the reader-pinning SeqCst Dekker pair becoming
adversarial against a drain — only becomes possible when writes go `&self`
(7c+), where its verification lands.

## Testing

- **Existing suite passes unchanged** — the receiver relaxation is backward-
  compatible (`&mut` callers still call `&self` methods; doctests using
  `let mut cache` still work).
- **Concurrent-reader stress test (the teeth).** Build and populate a cache with
  N known key→value pairs (`&mut` phase), then `thread::scope` with T threads
  sharing `&cache`, each doing M rounds of `get` / `get_no_freq_incr` on random
  known keys plus some absent keys. Assertions:
  - every lookup of a present key returns its correct, stable value (no writes
    during the concurrent phase, so values are fixed — a torn read, corrupted
    freq slot, or botched pin surfaces as a wrong value or crash);
  - absent keys consistently return `None`;
  - threads hold several `Item`s at once (ref_count on shared segments goes >1
    and back), exercising overlapping pins;
  - after the scope joins, every segment's `ref_count` is back to 0 (no leaked
    pins) and the cache still serves correctly (a follow-up `get` + `insert`
    work).
  Run in `--release` repeatedly to shake out interleavings; keep it deterministic
  in its assertions (values are fixed).
- **No loom model.** Concurrent-reads-only is plain atomic RMWs (freq CAS, pin
  incr/decr) with no Dekker/drain race — eviction is `&mut`, excluded while reads
  share `&cache`. The reader-pinning SeqCst Dekker pair is only adversarial once
  a writer/evictor races a pin (7c/7d), where loom for it belongs. A real-thread
  stress test is the right and sufficient tool for 7b.

## Non-goals / deferred

- Writes (`insert`, `cas`, `delete`, numeric ops) and eviction stay `&mut`
  (exclusive) — reader-vs-writer concurrency is 7c (eviction behind a Mutex) and
  7d (writer-vs-drain + generation seal CAS).
- `Segcache: Send` and `Arc`-shareability are 7e (with the full concurrent
  stress, including the racing-pin reader-safety test item 5b established needs
  `&self`).
- No production behavior change beyond the receiver relaxation.
