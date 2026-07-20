# Copy-Then-Publish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorder the two eviction copy paths (`Segment::copy_into`, `s3fifo_promote_from`) from publish-then-copy to copy-then-publish, closing the torn-read hole that opens once reads become `&self` (roadmap item 7a).

**Architecture:** Both sites currently `cas_location(old→new)` (publish) then `copy_nonoverlapping` (write bytes). A reader that Acquire-loads the published location could read the destination before the bytes exist. Reorder to write the bytes first, then publish via the existing Release-CAS `cas_location` — the Release orders the bytes ahead of the publish. Single-threaded behavior is identical; the fix is verified by a loom message-passing model (SC-independent, so loom-provable, unlike the reader-pinning SeqCst Dekker pairs). The API stays `&mut self`; concurrent reads are item 7b.

**Tech Stack:** Rust, `crate::sync` atomics (std/loom), loom for the ordering model.

**Spec:** `docs/superpowers/specs/2026-07-20-copy-then-publish-design.md`

**Branch:** `copy-then-publish` (already created; the spec commit is on it).

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

### Task 1: Reorder both copy sites to copy-then-publish

**Files:**
- Modify: `crates/segcache/src/segments/segment.rs` (`copy_into`, ~line 245)
- Modify: `crates/segcache/src/segments/segments.rs` (`s3fifo_promote_from`, ~line 1481)

This is a behavior-preserving reorder (single-threaded identical), so the existing suite is the harness. No new test in this task.

- [ ] **Step 1: Reorder `copy_into`**

In `segment.rs`, the current inner block (segment.rs:283-300) is:
```rust
            let new_loc = pack_location(target.id(), write_offset as u64);
            if hashtable.cas_location(item.key(), old_loc, new_loc, true) {
                unsafe {
                    std::ptr::copy_nonoverlapping(src, dst, item_size);
                }
                self.remove_item_at(read_offset);
                target.header.incr_live_items();
                target.header.incr_live_bytes(item_size as i32);
                target.set_write_offset(write_offset as i32 + item_size as i32);

                #[cfg(feature = "metrics")]
                {
                    items_copied += 1;
                    bytes_copied += item_size;
                }
            } else {
                return Err(SegmentsError::RelinkFailure);
            }
```
Replace with copy-then-publish (bytes written before the Release-CAS publishes):
```rust
            let new_loc = pack_location(target.id(), write_offset as u64);
            // Copy-then-publish: write the bytes into the destination BEFORE the
            // Release-CAS publishes the new location. The Release success ordering
            // on cas_location orders these writes ahead of the publish, so a
            // reader that observes new_loc (Acquire) always sees the copied bytes.
            // On CAS failure the bytes are orphaned at dst (write_offset is not
            // advanced, nothing points here), and we abort the copy.
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst, item_size);
            }
            if hashtable.cas_location(item.key(), old_loc, new_loc, true) {
                self.remove_item_at(read_offset);
                target.header.incr_live_items();
                target.header.incr_live_bytes(item_size as i32);
                target.set_write_offset(write_offset as i32 + item_size as i32);

                #[cfg(feature = "metrics")]
                {
                    items_copied += 1;
                    bytes_copied += item_size;
                }
            } else {
                return Err(SegmentsError::RelinkFailure);
            }
```
(The capacity check `write_offset + item_size >= target.data.len()` already happened earlier at segment.rs:275, so the copy at the granted `write_offset` is in-bounds.)

- [ ] **Step 2: Reorder `s3fifo_promote_from`**

In `segments.rs`, the current block (segments.rs:1481-1501) combines the capacity check with the CAS in one `&&`:
```rust
            if freq > 0 {
                let write_offset = dst.write_offset() as usize;
                let new_loc = pack_location(dst.id(), write_offset as u64);
                if write_offset + item_size < seg_size
                    && hashtable.cas_location(item.key(), old_loc, new_loc, true)
                {
                    unsafe {
                        let s = src.data_ptr().add(offset);
                        let d = dst.data_ptr().add(write_offset);
                        std::ptr::copy_nonoverlapping(s, d, item_size);
                    }
                    src.remove_item_at(offset);
                    dst.incr_live_items();
                    dst.incr_live_bytes(item_size as i32);
                    dst.set_write_offset(write_offset as i32 + item_size as i32);

                    #[cfg(feature = "metrics")]
                    ITEM_COMPACTED.increment();
                }
                // If no room in target, item stays in source and will be evicted.
            }
```
Split the `&&` so the capacity check gates the copy, then copy-then-publish:
```rust
            if freq > 0 {
                let write_offset = dst.write_offset() as usize;
                if write_offset + item_size < seg_size {
                    let new_loc = pack_location(dst.id(), write_offset as u64);
                    // Copy-then-publish (see copy_into): write bytes before the
                    // Release-CAS publishes new_loc. On CAS failure the bytes are
                    // orphaned (write_offset not advanced) and the item stays in
                    // src to be evicted — same outcome as before, minus the
                    // torn-read window.
                    unsafe {
                        let s = src.data_ptr().add(offset);
                        let d = dst.data_ptr().add(write_offset);
                        std::ptr::copy_nonoverlapping(s, d, item_size);
                    }
                    if hashtable.cas_location(item.key(), old_loc, new_loc, true) {
                        src.remove_item_at(offset);
                        dst.incr_live_items();
                        dst.incr_live_bytes(item_size as i32);
                        dst.set_write_offset(write_offset as i32 + item_size as i32);

                        #[cfg(feature = "metrics")]
                        ITEM_COMPACTED.increment();
                    }
                }
                // If no room in target, item stays in source and will be evicted.
            }
```

- [ ] **Step 3: Verify existing suite is unchanged**

Run:
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```
Expected: ALL PASS, no warnings, no diffs. The reorder is behaviorally identical single-threaded, so every merge / S3-FIFO / eviction test must pass unchanged. If any test fails, STOP and understand why — the reorder should not change any single-threaded outcome (report BLOCKED rather than adjusting tests).

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/segments/segment.rs crates/segcache/src/segments/segments.rs
git commit -m "Reorder eviction copy paths to copy-then-publish"
```
(with the standard footer)

---

### Task 2: Loom model for the copy-then-publish ordering

**Files:**
- Modify: `crates/segcache/src/hashtable/table.rs` (extend the existing `loom_tests` module) — OR a location where the real `cas_location` + `crate::sync` atomics are reachable.

The property is message-passing: Release-publish (after writing the payload) → Acquire-observe → read payload. This is SC-independent, so loom can verify it (unlike the reader-pinning SeqCst Dekker pairs). Use the REAL `cas_location` on a loom-instrumented `MultiChoiceHashtable` — the existing loom test at table.rs:1181 (`Arc::new(MultiChoiceHashtable::new(7))` + `thread::spawn` + `cas_location`) proves this is loom-tractable and is your structural template.

- [ ] **Step 1: Read the template** — the existing `#[cfg(all(test, feature = "loom"))] mod loom_tests` in `table.rs` (starts ~line 1170), especially the model at ~1181. Note how it constructs the hashtable, seeds an initial location, and races two threads doing `cas_location`. Note the `KeyVerifier` those tests use (there is likely a stub/always-true verifier for the hashtable's own loom/unit tests — reuse it).

- [ ] **Step 2: Write the ordering model**

Add a loom model `loom_copy_then_publish_no_torn_read` that mirrors `copy_into`'s publish:
- Shared: `Arc<MultiChoiceHashtable>` seeded so key K resolves to an OLD location; and a shared payload atom `Arc<loom::sync::atomic::AtomicU8>` representing the byte at the NEW destination, initialized to a NON-sentinel value (e.g. `0`).
- **Writer thread** (mirrors the reordered copy_into body): `payload.store(SENTINEL, Relaxed)` then `ht.cas_location(K, OLD, NEW, true)` (the real Release-CAS).
- **Reader thread** (mirrors a get): observe whether K now resolves to NEW — use whichever primitive is cleanest and faithful (`get_item_frequency(K, NEW).is_some()`, or a lookup with the stub verifier, or a direct check that the published slot equals NEW). If it observes NEW, `let b = payload.load(Acquire); assert_eq!(b, SENTINEL, "reader saw published location with unwritten payload")`.
- The assertion must hold in EVERY interleaving loom explores. Because the model is CAS + acquire/release message-passing (no Dekker/SB shape), it is SC-independent — no false loom violation.

If wiring the real hashtable observation under loom proves awkward (state-space or verifier friction), fall back to a faithful abstract model: a shared `AtomicU64` "location slot" + the `AtomicU8` payload, writer does `payload.store(SENTINEL, Relaxed); slot.compare_exchange(OLD, NEW, Release, Relaxed)`, reader does `if slot.load(Acquire) == NEW { assert payload.load(Acquire) == SENTINEL }`. Document in a comment that this mirrors `copy_into`/`s3fifo_promote_from`'s publish and why it's SC-independent. Prefer the real `cas_location` if tractable (it also guards cas_location's own Release ordering).

Test name MUST contain "loom" (CI filter is `--features loom -- loom`).

- [ ] **Step 3: Run the loom suite**

Run: `cargo test -p segcache --features loom -- loom`
Expected: PASS — the existing 15 models plus your new one (16). Report the exact count. Loom can be slow; if the model is pathologically slow, reduce the state space (fewer threads/values) rather than weakening the assertion, and report.

- [ ] **Step 4: Bite-check the model has teeth**

Temporarily revert the writer to publish-then-store (the OLD order): `ht.cas_location(...)` (or `slot.compare_exchange`) FIRST, then `payload.store(SENTINEL, Relaxed)`. Run `cargo test -p segcache --features loom -- loom_copy_then_publish`. Expected: FAIL — loom finds an interleaving where the reader observes NEW with `payload == 0`. Report the loom failure output. RESTORE by re-editing (never `git checkout`). Re-run → green.

- [ ] **Step 5: Battery + commit**

Run: `cargo test -p segcache`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo clippy -p segcache --all-targets -- -D warnings`, `cargo fmt --all --check` — all clean.

```bash
git add crates/segcache/src/hashtable/table.rs
git commit -m "Add loom model proving copy-then-publish closes the torn-read window"
```
(with the standard footer)

---

### Task 3: Doc cleanup + full verification battery

**Files:**
- Modify: `docs/superpowers/specs/2026-07-17-drain-safe-merge-design.md` (the item-5b design doc, now in main) — §1's deferred-to-item-7 note about copy_into ordering.

- [ ] **Step 1: Update the item-5b spec note**

In `docs/superpowers/specs/2026-07-17-drain-safe-merge-design.md`, find the §1 note that says the copy path publishes before writing bytes and that "reordering both call sites to copy-then-publish is an item-7 prerequisite." Update it to record that this was **resolved in item 7a** (this PR), with a pointer to `2026-07-20-copy-then-publish-design.md`. Keep it brief — one or two sentences; don't rewrite the section.

- [ ] **Step 2: Full verification battery**

```bash
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --features loom -- loom
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```
Expected: all PASS (16 loom models now), no warnings, no diffs. Report exact counts.

- [ ] **Step 3: Bench guard**

The reorder touches only the eviction copy inner loop (write-then-CAS vs CAS-then-write — same operations, reordered), and the read/reserve hot paths are untouched. Run the standard guards to confirm no regression:
```bash
cargo bench -p segcache -- set 2>&1 | tail -20
cargo bench -p segcache -- incr 2>&1 | tail -12
```
Record `set/1b/1b` and `incr/hot_counter` (baselines ~40ns / ~38ns). These don't exercise the copy paths, so expect no movement; report the numbers. (There is no eviction-specific criterion bench.)

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-07-17-drain-safe-merge-design.md
git commit -m "Record copy-then-publish as resolved (item 7a) in the item-5b spec note"
```
(with the standard footer)

---

### Task 4: Final review + finish

- [ ] **Step 1: Review the diff against the spec**

Run `git diff main --stat` and re-read `docs/superpowers/specs/2026-07-20-copy-then-publish-design.md`. Confirm: both copy sites reordered (copy-then-publish), single-threaded behavior preserved (existing tests green), a loom model verifies the ordering and was bite-checked, and no reorder was applied to `replace_at` (already correct).

- [ ] **Step 2: Final whole-branch review**

Dispatch a whole-diff adversarial review: confirm the reorder is correct at both sites (bytes written before the Release-CAS; CAS-failure path leaves no published dangling location and no reader hazard; capacity check still gates the copy at each site); confirm the loom model is SC-independent and genuinely exercises the ordering (not vacuous); confirm no single-threaded behavior change.

- [ ] **Step 3: Use the finishing-a-development-branch skill**

Invoke `superpowers:finishing-a-development-branch` to push and open a PR against `pelikan-io/cache-rs` (cross-fork: `--repo pelikan-io/cache-rs --head brayniac:copy-then-publish`), matching how items 4/5b landed (#30/#31). The PR body should state plainly that this closes the deferred copy-ordering prerequisite and that the end-to-end concurrent racing-pin stress remains item 7e (needs a shareable cache) — no over-claiming.
