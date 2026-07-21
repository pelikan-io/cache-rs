# `&self` Reads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Flip the read methods from `&mut self` to `&self`, establish `Segcache: Sync`, and add a concurrent-reader stress test (roadmap item 7b).

**Architecture:** `Segcache::get`/`get_no_freq_incr` bodies already use only `&self` operations (atomic hashtable lookup, `acquire_item_at` pinning, `generation`), so the flip is a receiver change. `Segcache: Sync` is locked in with a compile-time guard (auto-derive expected — the hashtable already carries `unsafe impl Send + Sync`). Writes and eviction stay `&mut` (exclusive), so reader-vs-writer concurrency is still impossible; this delivers concurrent readers on a shared cache. Verified by a real-thread stress test (no loom — reads-only is plain atomics, no Dekker race).

**Tech Stack:** Rust, `std::thread::scope` for the concurrent-reader test.

**Spec:** `docs/superpowers/specs/2026-07-21-self-reads-design.md`

**Branch:** `self-reads` (already created; the spec commit is on it).

**Conventions:**
- New files get NO license header (Pelikan-only policy).
- All commits end with:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017xPi3BW7qJUxXxX9Pcjm7w
  ```
- Run commands from repo root `/Users/brian/workspace/brayniac/cache-rs`.
- Bite-checks: restore by re-editing, NEVER `git checkout <file>`.
- CI enforces `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all --check`. Also run `cargo clippy -p segcache --all-targets -- -D warnings` (default features) — CI's `--all-features` run skips `#[cfg(not(feature="loom"))]` modules (a gap found in item 5b).

---

### Task 1: Flip read receivers to `&self` and lock in `Sync`

**Files:**
- Modify: `crates/segcache/src/segcache.rs` (`get`, `get_no_freq_incr`, `items`, `check_integrity`, + the `Sync` guard)
- Modify: `crates/segcache/src/segments/segments.rs` (`items` → `&self`)

Behavior-preserving receiver relaxation — existing tests are the harness. No new test in this task.

- [ ] **Step 1: Make `Segments::items` take `&self`**

Current (segments.rs:1622-1637) uses `get_mut` (only for `check_magic` + `live_items`). Rewrite to read the atomic headers directly — no `&mut`, no data-slice view:
```rust
    /// Count the total number of live items across all segments.
    #[cfg(any(test, feature = "debug"))]
    pub(crate) fn items(&self) -> usize {
        let mut total = 0;
        for idx in 0..self.cap as usize {
            let count = self.headers[idx].live_items();
            debug!("{count} items in segment {}", idx + 1);
            total += count.max(0) as usize;
        }
        total
    }
```
Note: the old version also called `segment.check_magic()` (a magic-byte integrity assertion). That is dropped here — `items()` is a counting helper, and magic verification lives in `check_integrity()`. This is an intentional, minor debug-only change; note it in your report.

- [ ] **Step 2: Flip the Segcache read receivers**

In `segcache.rs`, change these four signatures from `&mut self` to `&self` (bodies unchanged):
- `pub fn get(&self, key: &[u8]) -> Option<Item>` (was `&mut self`, ~line 80)
- `pub fn get_no_freq_incr(&self, key: &[u8]) -> Option<Item>` (~line 114)
- `pub fn items(&self) -> usize` (debug-gated, ~line 62) — now calls the `&self` `Segments::items()`
- `pub fn check_integrity(&self) -> Result<(), SegcacheError>` (`#[cfg(feature = "debug")]`, ~line 560) — its body already calls `self.segments.check_integrity(&self.hashtable)` which is `&self`

Do NOT change any other method. Verify `get`/`get_no_freq_incr` bodies compile unchanged under `&self` (they use `self.verifier()`, `self.hashtable.lookup`, `self.segments.acquire_item_at`, `self.segments.generation` — all `&self`).

- [ ] **Step 3: Add the `Sync` compile-time guard**

In `segcache.rs`, right after the `struct Segcache { ... }` definition (~line 22), add:
```rust
// Compile-time guard: Segcache must be Sync so &Segcache can be shared across
// threads for concurrent reads (item 7b). This relies on auto-derive — the
// hashtable carries its own `unsafe impl Send + Sync` for its raw-pointer
// internals, and every other field is a Sync type (anonymous mmap, atomic
// headers, lock-free Injector queues, Xoshiro RNG, atomic TTL-bucket links).
// A future !Sync field breaks the build here rather than silently at 7e.
const _: () = {
    fn assert_sync<T: Sync>() {}
    let _ = assert_sync::<Segcache>;
};
```
If this FAILS to compile, a field is `!Sync`. Identify it (`cargo build -p segcache` names the type), and — only for the specific blocking field's containing type — add a justified `unsafe impl Sync` following the `MultiChoiceHashtable` precedent (table.rs:35-36), documenting why `&self` access to that field is thread-safe. Report which field blocked and how you resolved it. (Expected: it compiles with no unsafe impl needed.)

- [ ] **Step 4: Verify existing suite unchanged**
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```
ALL must pass/clean. The receiver relaxation is backward-compatible (existing `let mut cache; cache.get(...)` calls still work). If a test or doctest fails to compile because it relied on `get` taking `&mut`, that is unexpected — report it (do not weaken; `&self` is strictly more permissive at call sites).

- [ ] **Step 5: Commit**
```bash
git add crates/segcache/src/segcache.rs crates/segcache/src/segments/segments.rs
git commit -m "Flip read methods to &self; lock in Segcache: Sync"
```
(with the standard footer)

---

### Task 2: Concurrent-reader stress test

**Files:**
- Modify: `crates/segcache/src/tests.rs` (add the test) — or a new `#[cfg(all(test, not(feature = "loom")))]` module if that fits the file's structure better.

This is the teeth — the first time the read path runs truly concurrently.

- [ ] **Step 1: Write the test**

Model construction on the existing tests in `tests.rs` (`Segcache::builder().segment_size(4096).heap_size(...).eviction(Policy::Fifo).build()`). `Item::value()` returns a `Value` that compares to byte literals (`item.value() == b"..."`, as in the doctests).

```rust
#[test]
fn concurrent_readers_see_correct_values() {
    // Populate a cache with known key→value pairs (&mut phase), then share
    // &cache across threads for a read-only concurrent phase. No writes happen
    // during the concurrent phase, so every present key has a fixed value —
    // any torn read, corrupted freq slot, or botched pin surfaces as a wrong
    // value or a crash.
    const KEYS: usize = 500;
    const THREADS: usize = 8;
    const ROUNDS: usize = 4_000;

    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("build cache");

    // key i -> value "val-<i>" (distinct per key so a mismatch is detectable).
    let key = |i: usize| format!("k{i:06}").into_bytes();
    let val = |i: usize| format!("val-{i:06}").into_bytes();
    for i in 0..KEYS {
        cache
            .insert(&key(i), val(i).as_slice(), None, Duration::ZERO)
            .expect("insert");
    }

    // Sanity: all present before the concurrent phase.
    for i in 0..KEYS {
        let item = cache.get(&key(i)).expect("present");
        assert!(item.value() == val(i).as_slice());
    }

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let cache = &cache; // shared &Segcache — requires Segcache: Sync
            s.spawn(move || {
                for r in 0..ROUNDS {
                    // Deterministic pseudo-random index, varied per thread.
                    let i = (t * 31 + r * 17) % KEYS;
                    // Hold two pins at once to exercise overlapping ref_counts.
                    let a = cache.get(&key(i)).expect("present key must be found");
                    assert!(a.value() == val(i).as_slice(), "torn/wrong value for key {i}");

                    let j = (i + 7) % KEYS;
                    let b = cache.get_no_freq_incr(&key(j)).expect("present");
                    assert!(b.value() == val(j).as_slice());

                    // An absent key is consistently None.
                    assert!(cache.get(b"definitely-absent-key").is_none());

                    drop(a);
                    drop(b);
                }
            });
        }
    });

    // After joining: pins are all released and the cache still serves.
    for i in 0..KEYS {
        let item = cache.get(&key(i)).expect("still present after concurrent reads");
        assert!(item.value() == val(i).as_slice());
    }
    // A write still works (exclusive &mut, borrow checker guaranteed the scope ended).
    cache
        .insert(b"post", b"ok", None, Duration::ZERO)
        .expect("insert after concurrent reads");
    assert!(cache.get(b"post").unwrap().value() == b"ok".as_slice());
}
```
Adjust imports (`std::time::Duration`, `Policy`, `Segcache`) to match how `tests.rs` imports them. Ensure all 500 keys actually fit in 64 segments (500 small items × ~20 bytes ≈ 10KB, well under 64×4096 = 256KB — no eviction, so nothing is dropped; if the fit is tight under the `integrity` feature, reduce KEYS or raise segments and note it). If any `insert` evicts (it must not for this test's invariant), the assertion "present key must be found" would catch it — size so eviction never triggers.

- [ ] **Step 2: Run repeatedly (shake out interleavings)**
```
cargo test -p segcache concurrent_readers_see_correct_values --release
for i in $(seq 1 20); do cargo test -p segcache concurrent_readers_see_correct_values --release 2>/dev/null | grep -q "test result: ok" || { echo "FAIL run $i"; break; }; done; echo done
```
Expected: 20/20 PASS. Also run once under `--features debug` (integrity magic checks on the read path).

- [ ] **Step 3: Bite-check the test has teeth**

Confirm the test actually depends on `Segcache: Sync` and on read correctness:
- Teeth for Sync/sharing: temporarily remove the `Sync` guard is not enough (auto-Sync remains). Instead confirm the test genuinely shares `&cache` across threads — it will not compile if `Segcache` is `!Sync`. Note this dependency (the concurrent test IS the consumer that would break if a future change makes the cache `!Sync`).
- Teeth for read correctness: temporarily corrupt the expected value in one assertion (e.g. compare against `val(i + 1)`) and confirm the test FAILS, then restore by re-editing. This proves the value assertions are live. Report the observed failure.

- [ ] **Step 4: Battery + commit**
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```
All clean.
```bash
git add crates/segcache/src/tests.rs
git commit -m "Add concurrent-reader stress test on a shared &Segcache"
```
(with the standard footer)

---

### Task 3: Final review + finish

- [ ] **Step 1: Full verification battery + review the diff against the spec**
```
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --features loom -- loom
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```
All pass (loom 16, unchanged — no loom in 7b). Re-read `docs/superpowers/specs/2026-07-21-self-reads-design.md`: confirm the four read methods are `&self`, the `Sync` guard is present, the concurrent-reader test exists and has teeth, and writes/eviction stayed `&mut` (grep `pub fn` in segcache.rs — `insert`/`cas`/`delete`/`wrapping_add`/`saturating_sub`/`try_into_numeric`/`expire`/`clear` must still be `&mut self`).

Optionally run the bench guard (`set/1b/1b`, `incr` — expect no movement; reads flip doesn't touch the write hot path).

- [ ] **Step 2: Final whole-branch review**

Dispatch a whole-diff adversarial review: confirm the `&self` flip is sound (no `&self` read method touches non-atomic mutable state); confirm `Segcache: Sync` is justified (auto-derive holds, or the unsafe impl — if any — is correctly reasoned); confirm the concurrent-reader test genuinely exercises concurrent reads with correct-value teeth and no leaked pins; confirm no write/eviction method was accidentally relaxed to `&self` (which would be unsound today).

- [ ] **Step 3: Use the finishing-a-development-branch skill**

Invoke `superpowers:finishing-a-development-branch` to push and open a PR against `pelikan-io/cache-rs` (cross-fork: `--repo pelikan-io/cache-rs --head brayniac:self-reads`), matching items 4/5b/7a (#30/#31/#32). The PR body should state that this enables concurrent readers on a shared cache (writes stay exclusive `&mut`), and that reader-vs-writer concurrency (7c/7d) and `Arc`/`Send` (7e) are still ahead.
