# Generation-Tagged Locations Implementation Plan (#50)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `Location` incarnation identity so a stale location cannot silently resolve to a different item at the same address.

**Architecture:** A 6-bit tag carved out of the 44-bit location's segment-id field, sourced from the segment header's `generation`. Because the tag rides inside the packed hashtable slot word, every existing compare-exchange on a published entry validates it for free. Requires first moving the generation bump from `try_reserve` to the state transitions that end a *used* incarnation. Full reasoning: `docs/superpowers/specs/2026-08-19-generation-tagged-locations-design.md` — **read it before starting**; it is the contract, and its five-site table and reconstruction constraint are load-bearing.

**Tech Stack:** Rust; `crate::sync` atomics; loom; criterion.

**Context for workers with zero repo knowledge:**
- `crates/segcache` is a concurrent segment-structured cache. A hashtable slot is a packed `u64`: `tag(12) | freq(8) | location(44)`. A `Location` is `seg_id(24) | offset>>3 (20)` — see `crates/segcache/src/hashtable/mod.rs` (`pack_location`/`unpack_location`) and `crates/segcache/src/hashtable/location.rs`.
- Segments cycle `Free → Reserved → Linking → Live → Sealed → Draining → Free` (plus `Relinking` for merge destinations and `AwaitingRelease` for reader-pinned condemned segments). `SegmentHeader::generation` (`crates/segcache/src/segments/header.rs`) counts incarnations.
- **Sequencing:** PRs #63 (external, fixes a use-after-free, touches `header.rs`), #66 (version bump) and #67 (loom `KeyOracle` fixture) are open. #63 ships first — expect a small conflict in `try_release_condemned` and rebase onto it. #67's fixture is needed only by Task 6; if it has not merged when you reach that task, say so and stop rather than duplicating it.
- Gates (all must be clean): `cargo test --workspace`, `cargo test -p segcache --features debug`, `cargo test -p segcache --release`, `cargo test -p segcache --features loom -- loom`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo clippy -p segcache --all-targets -- -D warnings`, `cargo fmt --all --check`.
- Process: bite-checks restore by re-editing, NEVER `git checkout <file>`. Never weaken an assert to get green — report BLOCKED with observations instead.

---

## File Structure

- Modify: `crates/segcache/src/segments/header.rs` — bump relocation, `try_release_drained` (Tasks 2)
- Modify: `crates/segcache/src/segments/segments.rs` — `recycle` uses the new transition; `resolve` (Tasks 2, 4)
- Modify: `crates/segcache/src/hashtable/mod.rs`, `location.rs` — layout, `pack_location` signature, capacity (Task 3)
- Modify: `crates/segcache/src/segcache.rs`, `segments/segment.rs` — call sites and validation policy (Tasks 3, 4)
- Modify: `crates/segcache/src/hashtable/table.rs` — loom models (Task 6)

---

### Task 1: Worktree and baseline

- [ ] **Step 1:** `git worktree add .worktrees/tagged-locations -b tagged-locations` from current `upstream/main`, then `cd` into it.
- [ ] **Step 2:** Run `cargo test -p segcache` and record the pass count as the baseline. Report it.
- [ ] **Step 3:** Read the design doc named above, in full.

---

### Task 2: Move the generation bump (the prerequisite)

**Files:** `crates/segcache/src/segments/header.rs`, `crates/segcache/src/segments/segments.rs`

- [ ] **Step 1: Write the failing tests first.**

Two behaviours, in `header.rs`'s or `segments.rs`'s test module (match where the neighbouring segment-state tests live):

1. `election_loser_release_does_not_bump_generation` — reserve a segment from the free queue, capture `generation()`, call the `release_unused` path, re-reserve the same id, and assert the generation is **unchanged** across the round trip. (Today it advances twice; this is the red.)
2. `used_segment_recycle_bumps_generation` — take a segment through reserve → fill/seal → drain → recycle and assert the generation advanced **exactly once** per full lifecycle.

Run both; (1) must FAIL on current code, (2) may already pass. Report the exact failure. If (1) passes, stop and report BLOCKED — the premise is wrong and the design needs revisiting.

- [ ] **Step 2: Add `SegmentHeader::try_release_drained()`** — `Draining → Free` (clearing links, as `recycle`'s inline `cas_metadata` does today), bumping `generation` on success. Doc-comment it as one of the two transitions that end a used incarnation, naming the other.

- [ ] **Step 3: Bump in `try_release_condemned()`** on success (`AwaitingRelease → Free`). Doc-comment that this one line covers all three condemned paths — the last reader's guard drop, `condemn`'s race-fix, and #63's `ReleaseCondemned` arm — because all three reach the transition.

- [ ] **Step 4: Remove the bump from `try_reserve`** (`header.rs`, the `generation.fetch_add` next to `reset_write_stats`/`mark_created`/`mark_merged`). Leave everything else in that function alone.

- [ ] **Step 5: `recycle` calls `try_release_drained()`** instead of its inline `cas_metadata(Draining, Free, ...)`, keeping its existing `debug_assert!` that the transition succeeded.

- [ ] **Step 6:** Both tests green. Then re-verify the three production readers of `generation` still behave — run the existing tests that cover them and name them in your report: the CAS-token tests in `cas.rs`, `try_expand_bails_on_stale_generation` in `ttl_bucket.rs`, and the `delete` pin-fail coverage in `pin_failure_tests.rs`.

- [ ] **Step 7:** Full gate set. Commit: `segcache: bump segment generation when a used incarnation ends, not on reserve`, with a body explaining the election-loser cheap-bump path and the three readers. Standard trailers (blank line, then `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01FuDgaMXJWz1fnYGhYF7TLh`).

---

### Task 3: Bit layout and `pack_location`

**Files:** `crates/segcache/src/hashtable/mod.rs`, `location.rs`, plus every `pack_location` call site

- [ ] **Step 1: Repartition.** `Location` becomes `seg_id(18) | tag(6) | offset>>3 (20)`. Keep `Location` opaque: add `fn tag(&self) -> u8`; `unpack_location` keeps returning `(seg_id, offset)`; **do not** add a way to rebuild a `Location` from parts other than `pack_location`.

- [ ] **Step 2: `pack_location(seg_id, generation, offset)`** — generation passed explicitly, masked to 6 bits internally. Doc-comment the reconstruction precondition: callers that rebuild a *published* location (`Segment::copy_into`, `Segments::s3fifo_promote_from`) must pass the generation of the incarnation that published it, which is sound there only because the drain owns the segment so its generation cannot advance underneath them.

- [ ] **Step 3: Update all 5 production `pack_location` call sites** (`segcache.rs` ×2, `segments.rs` ×3 — grep to confirm the current set) to pass the correct generation. For the two reconstruction sites, read the generation from the segment they already hold.

- [ ] **Step 4: Capacity assertion.** At `Segments` construction, assert the configured segment count fits in 18 bits (one below the field's capacity, the top id being reserved for the ghost sentinel), with an error naming the limit and pointing at `segment_size` as the lever. This must be a real error or panic at build time, not a debug assert — a silently oversized heap aliases.

- [ ] **Step 5: Bit-layout unit tests** — round-trip across id/tag/offset boundaries including maximum values; `Location::GHOST` still distinguishable from every real location; the capacity assertion fires at the limit and not below it.

- [ ] **Step 6:** Full gates, commit.

---

### Task 4: Validation

**Files:** `crates/segcache/src/segments/segments.rs`, `crates/segcache/src/segcache.rs`

- [ ] **Step 1: `Segments::resolve(location) -> Option<(NonZeroU32, usize)>`** — compares the location's tag against the live header generation, returns `None` on mismatch. Document that `None` always means "this location is no longer yours", never an error.

- [ ] **Step 2: Apply the design's mismatch policy** at each site in its table: `get_pinned` (treat as a miss, retry the lookup), `remove_at` and the unpinned-unlink arms (skip the decrement — the incarnation that owned it is gone and its counters were reset wholesale), `acquire_item_at` (fail the pin; callers already handle it), eviction/merge relocation (skip the item, as a lost relink CAS does today). Do not invent new error paths.

- [ ] **Step 3: Deterministic test that relocation still relinks.** A merge must still relocate items after the layout change. Assert it directly — a silent no-op here degrades merges without failing anything. Verify red by passing a deliberately wrong generation at one reconstruction site (re-edit, then restore).

- [ ] **Step 4: A stale-location test** — construct a location, recycle its segment, assert `resolve` rejects it and that the corresponding public operation behaves per the policy above.

- [ ] **Step 5:** Full gates, commit.

---

### Task 5: Benchmark A/B

- [ ] **Step 1:** `cargo bench -p segcache --bench benchmark -- --save-baseline pre` on `upstream/main` (separate worktree), covering `get/1b/0b`, `set/1b/64b`, `set_fresh`, `hot_counter`.
- [ ] **Step 2:** Same benches on the branch with `--baseline pre`.
- [ ] **Step 3:** Report every number and criterion's verdict verbatim. The unpack gains a mask and compare on the read path; #60 showed a 1-2% regression can appear from a change that looks free. If `get` regresses meaningfully, say so plainly rather than explaining it away — a `#[cold]` split or a cheaper validation order may be needed.

---

### Task 6: Loom models (requires #67 merged)

- [ ] **Step 1:** Confirm #67's `KeyOracle` fixture is on `main`. If not, STOP and report — do not reimplement it.
- [ ] **Step 2:** Model recycle-and-refill producing a stale location and assert every consumer rejects it.
- [ ] **Step 3:** Per #67's discipline, every new model must be proven to FAIL against neutered code. Report the neutering used and the observed failure for each. A model that cannot be made to fail is not evidence.
- [ ] **Step 4:** Full gates including `--features loom -- loom`, commit.

---

### Task 7: Review and PR

- [ ] **Step 1:** Controller runs an adversarial review focused on: whether any path can still reuse a segment without a bump (the five-site table is the checklist); whether the two reconstruction sites can ever see an advanced generation; whether 6 bits holds given the corrected bump semantics; and whether any `Location` can be built bypassing `pack_location`.
- [ ] **Step 2:** Rebase onto whatever of #63/#67 has landed. Open the PR to `pelikan-io/cache-rs` main, titled `segcache: generation-tagged locations (fixes #50)`, with the bump change presented as the prerequisite it is, the capacity change stated plainly, the benchmark table, and the metrics-consolidation ordering note from the design's §2.

---

## Self-review notes

- Design §2 prerequisite → Task 2; layout + reconstruction constraint → Task 3; validation policy table → Task 4; §4 testing → Tasks 3-6.
- Task 2 is independently correct and independently reviewable; if #50 is ever abandoned, that commit still stands on its own.
- The capacity assertion is the one user-visible breaking change: heaps above 262,142 segments now fail at construction. That belongs in the PR body, not buried.
