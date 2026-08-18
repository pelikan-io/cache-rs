# Fresh-key insert de-duplication (item 7f tracked follow-up)

**Status:** design approved 2026-08-18
**Predecessor:** item 7f (PR #36) fixed the *overwrite* half of duplicate-publish (F4 matching-slot retry) and left the *fresh-key* half as a tracked follow-up (`table.rs` NOTE near `test_concurrent_same_key_insert_no_duplicates`; `segcache.rs` fresh-key arm comment).

## 1. Problem

`Segcache::insert` on a key with no live hashtable entry calls `MultiChoiceHashtable::insert` → `try_link_in_bucket`, which has a TOCTOU across its passes:

1. **Pass 1** scans both candidate buckets for an existing entry or matching ghost — finds nothing (the key is fresh).
2. **Pass 2** claims an *empty* slot via `compare_exchange(0, new_packed)`. **Pass 3** claims any ghost slot the same way.

Two threads inserting the same fresh key can both complete pass 1 before either publishes, then each win a claim CAS on **different** slots — different slots of one bucket, or the key's two *different* candidate buckets (`num_choices = 2`, so no single-bucket reasoning covers it). Result: **two live hashtable entries for one key**. The F4 fix does not help here: F4's same-slot retry only fires when pass 1 *finds* a matching slot.

### Consequences

Byte-neutral — each location is counted once and decremented exactly once; no crash, no leak (verified in 7f). The defects are semantic:

- **Delete resurrection** (the user-visible anomaly): `remove`/`try_unlink_in_bucket` unlinks only the *first* matching entry in scan order. Deleting the key exposes the shadowed duplicate — the key comes back with the losing insert's value. `cas()` and overwrites have the same hazard: they hit the first-found slot and the duplicate can resurface later.
- **Slot waste**: the shadowed entry occupies a bucket slot until its segment is evicted or expires.
- **Test contortions**: the 7f stress tests must seed keys single-threaded (and keep freq > 0) purely to dodge this window, and cannot assert exact item counts.

A same-class narrow gap rides along (documented in `segcache.rs`, fresh-key arm): when `hashtable.insert` returns `Ok(Some(raced_old))`, the unlink has already happened before the pin attempt, so a drain claiming that segment in between skips the per-item decrement. That is accounting-benign (the drain resets the segment's counters wholesale) and stays out of scope; this spec cites it only because the fix shrinks how often that path is taken.

## 2. Goal and approach decision

**Goal (agreed):** full insert-if-absent atomicity — *at most one live hashtable entry per key, always*; concurrent fresh inserts linearize.

**Approach (agreed): prototype-and-measure, phased.**

- **Phase 1 (build now):** striped lock on the fresh-key path. Simple, provably correct, loom-verifiable.
- **Benchmark gate:** clean same-machine A/B (§6). Brian calls the verdict on the numbers.
- **Phase 2 (build only if the gate fails):** lock-free claim-then-resolve (§5), designed here so the swap is mechanical. Same public behavior either way — the internals swap invisibly to `segcache.rs`.

This mirrors the 7c precedent: accept a pragmatic coarse lock where fully-lock-free buys little, but keep the lock off every hot path except the one with the bug.

## 3. Phase 1 design — striped insert lock

### Where

Inside `MultiChoiceHashtable` (`table.rs`), not `Segcache`. "At most one live entry per key" is a hashtable-layer invariant, and the hashtable's only entry-*creating* paths are `try_link_in_bucket` passes 2 and 3. Everything else (`cas_location`/`cas_location_at` relocation, `remove`, `convert_to_ghost`) mutates or removes existing entries only. Fixing it here keeps the invariant enforceable and testable at one layer (`MockVerifier` infra, loom).

### Structure

- A fixed array of **1024 `CachePadded<std::sync::Mutex<()>>`** stripes on the hashtable (crossbeam-utils is already a dependency; `std::sync::Mutex` matches the `chain_lock` precedent; `CachePadded` prevents false sharing between adjacent stripes).
- Stripe index = **key hash & 1023**, using the same hash `probe()` already computes — both candidate buckets of a key derive from that one hash, so one stripe covers both. Small refactor: `probe` (or a sibling) exposes the raw hash alongside `(tag, buckets)`. No second hash computation.
- Stripe count is fixed (not scaled with `hash_power`): contention requires two *concurrent fresh inserts* whose hashes collide mod 1024 — rare, and the cost of a collision is a short wait, not incorrectness.

### Protocol

`insert()` restructures around a split of `try_link_in_bucket` into:

- `try_replace_existing` — pass 1 only: existing-entry / matching-ghost CAS-replace with the F4 same-slot retry;
- `try_claim_new_slot` — passes 2 + 3: empty-slot then ghost-slot claim.

Flow:

1. **Lock-free fast path (unchanged):** `try_replace_existing` across both candidate buckets. Any hit resolves exactly as today. This is the overwrite path — zero new cost.
2. **Fresh-key slow path (new):** no match →
   a. acquire the key's stripe lock;
   b. **re-run `try_replace_existing` under the lock** — a racing fresh-inserter may have published while we waited; a hit resolves to a replace, returning the racer's location as `Ok(Some(raced_old))` (the `segcache.rs` fresh-key arm already handles that shape);
   c. only then `try_claim_new_slot` (least-full-bucket targeting preserved);
   d. release. `Err(())` (bucket full) propagates as today → `rollback_reservation`.

The replace CASes remain CAS loops even under the lock: readers' freq bumps still race individual slot words; the lock serializes only *entry creation*, not slot mutation.

### Correctness argument

- **Single-entry invariant:** every entry-creating claim now happens inside the key's stripe critical section, preceded by an in-section absence re-check. Two fresh inserters of one key serialize; the second sees the first's entry and replaces it in place. No path creates an entry for a key that already has one.
- **Deadlock-freedom:** the stripe lock is a **leaf lock**. The critical section is pure bucket-word CAS + `verifier.verify` (reads of segment memory) — it never touches `chain_lock`, the eviction policy mutex, `remove_at`, or any pin-wait. Lock inventory (7c spec) gains one line: *stripe lock — innermost; never held across any other lock, pin acquisition, or wait*. The WriterPin held across `hashtable.insert` (item 7d, H2) is a pin, not a lock, and drains waiting on `active_writers` never take stripe locks — no new cycle.
- **Delete concurrent with fresh insert:** a delete that lands before the in-section re-check makes the key absent → the inserter claims a slot (linearizes delete-then-insert). After the claim, delete unlinks the sole entry (insert-then-delete). Both valid; no duplicate either way.
- **Linearization point** unchanged: the Release CAS that publishes `new_packed` (spec §4 of concurrent-reserve) — the lock adds mutual exclusion around it but is not itself the publish.

### What it costs

One uncontended `Mutex` lock/unlock pair on **fresh inserts only** (the miss-fill path) plus one extra pass-1 scan of both buckets (the under-lock re-check). Overwrites, `get`, `delete`, `cas`, numeric ops: untouched.

## 4. Consequential cleanups (Phase 1 scope)

- **Un-seed the stress tests:** `eviction_concurrency_tests.rs` tests that seed keys single-threaded solely to dodge this window drop the workaround where their asserts permit; comments referencing the "tracked fresh-key duplicate-publish follow-up" (tests 8/9 area) update to reference this fix.
- **Stale comments:** the `table.rs` NOTE (fresh-key race "flagged for a follow-up item") and the `segcache.rs` fresh-key-arm comment rewrite to describe the new protocol.
- **Out of scope:** the drain-vs-raced-old decrement gap (accounting-benign, §1); `delete`-all-matches sweeping (unnecessary once duplicates cannot exist); CLAUDE.md concurrency-section refresh (separate housekeeping).

## 5. Phase 2 contingency — lock-free claim-then-resolve

Built **only** if the §6 gate fails. Recorded here so the decision is pre-made.

1. **Claim** exactly as today: pass 1 misses, CAS-claim an empty/ghost slot at position **P** = (choice index, slot index).
2. **Resolve:** re-scan both candidate buckets for another live same-tag entry that verifies for our key at **Q ≠ P**. None → done (common case: one extra two-bucket read scan, no atomics).
3. **Conflict:** deterministic winner both racers compute identically: **lowest (choice index, slot index) wins.**
   - Winner: done; the loser is responsible for retracting.
   - Loser: **retract** its own claim (CAS-loop the slot back to 0 — a loop because a concurrent `get` may have freq-bumped the entry), then **convert to a replace** on the winner's slot (F4 same-slot retry CAS), returning the winner's location as `raced_old`. Final state: one entry holding the loser's value — linearizes as winner's-insert-then-loser's-overwrite, a valid order for two concurrent inserts.
4. **Interference:** if the winner's slot changes under the loser's replace CAS (delete, eviction relocation), the loser re-runs `insert` from the top — retraction already restored single-entry, so the retry is clean.

Known proof obligations (why this is not Phase 1): transient reader visibility of the loser's entry (benign — reads linearize before the retraction — but must be argued), N-way race termination (pairwise lowest-wins resolution; needs a bounded-retry argument), and the loom SeqCst limitation (the mutual-exclusion half is stress-test-only, not model-checkable).

## 6. Testing and benchmark gate

### Correctness (Phase 1)

- **Unseeded fresh-key race test** (hashtable layer): the `table.rs` NOTE scenario — N threads race the very first insert of a key (no seed entry), assert exactly one live entry per trial, many trials. **Bite-check:** temporarily remove the stripe lock (restore by re-edit, never `git checkout`) and confirm this test fails.
- **Resurrection test** (segcache layer): threads race fresh inserts of one key; then `delete()` → `get()` must return `None`.
- **loom model:** two threads insert the same fresh key through the real locked path, assert single-entry. A mutex-serialized invariant is SC-independent — genuinely loom-verifiable, unlike the SeqCst Dekker pairs.
- Existing suites stay green: 19 loom models, concurrent stress (release + debug), `clippy --all-targets` both with default features and `--all-features` (CI gap: loom-gated modules escape the all-features run).

### Benchmark gate

- **New `set_fresh` bench:** monotonically unique keys so every op is a genuine fresh insert. The existing `set` bench cycles 1M keys and is mostly overwrites after warmup — it dilutes exactly the changed path.
- **Clean same-machine A/B:** baseline `main` vs the branch — `set`, `set_fresh`, `get`, `incr` (`get`/`incr` are no-regression sentinels; they must not move).
- **Gate:** Brian reviews the numbers. Working prior: uncontended lock/unlock ≈ 5–10 ns on a ~45 ns insert; a `set_fresh` regression beyond ~10% or any movement in `set` triggers Phase 2.
