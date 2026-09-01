# Fresh-Key Insert De-duplication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Guarantee at most one live hashtable entry per key under concurrent fresh-key inserts (the item-7f tracked follow-up), via a striped insert lock serializing entry creation, then benchmark the miss-fill cost against the pre-fix baseline.

**Architecture:** All entry *creation* (empty-slot claim, ghost takeover) in `MultiChoiceHashtable` moves under a per-key-hash striped `Mutex` with an under-lock absence re-check; the overwrite fast path stays lock-free. `try_link_in_bucket` splits into `try_replace_existing` (live-entry replace, F4 retry) and `try_claim_new_slot` (all creation forms), and `insert()` scans ALL candidate buckets for a live match before any claim — which also fixes a latent single-threaded cross-bucket duplicate. Spec: `docs/superpowers/specs/2026-08-18-fresh-key-dedup-design.md`. Phase 2 (lock-free claim-then-resolve) is NOT in this plan — it is built only if Brian rejects the Task 7 benchmark numbers.

**Tech Stack:** Rust; `std::sync::Mutex`/`loom::sync::Mutex` via `crate::sync`; `crossbeam_utils::CachePadded`; criterion benches; loom.

**Context for workers with zero repo knowledge:**
- The crate is `crates/segcache`. The hashtable is `crates/segcache/src/hashtable/table.rs` (`MultiChoiceHashtable`): buckets of 8 packed `AtomicU64` slots (`Hashbucket`), each slot = 12-bit tag + 8-bit freq + 44-bit location; a key hashes once (`probe`) to a tag and `num_choices` (default 2) candidate buckets. `KeyVerifier::verify(key, location, allow_deleted)` checks that a location really holds the key (segment memory read; tests use in-module `MockVerifier`).
- "Ghost" slots (S3-FIFO tombstones) carry a tag + freq but no live item; taking one over CREATES a live entry, same as claiming an empty slot.
- `crate::sync` (`crates/segcache/src/sync.rs`) re-exports std vs loom atomics behind the `loom` cargo feature. Loom models live in `mod loom_tests` at the bottom of `table.rs` and run in CI as `cargo test -p segcache --features loom -- loom`. The loom stripe array is shrunk (const) because loom tracks every sync object.
- Build/lint gates (CI enforces all): `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, **also** `cargo clippy -p segcache --all-targets -- -D warnings` with default features (loom-gated test modules escape the all-features run), `cargo fmt --all --check`.
- Process rule: when temporarily breaking code to prove a test fails (bite-check), restore by re-editing — NEVER `git checkout <file>` (it reverts to the last commit and destroys uncommitted work).

---

## File Structure

- Modify: `crates/segcache/benches/benchmark.rs` — add `set_fresh` bench (Task 1)
- Modify: `crates/segcache/src/sync.rs` — re-export `Mutex` (Task 2)
- Modify: `crates/segcache/src/hashtable/table.rs` — the whole fix + hashtable-layer tests + loom model (Tasks 3-5)
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs` — segcache-level resurrection test + module-note update (Task 6)
- Modify: `crates/segcache/src/segcache.rs` — comment updates only, no code change (Task 6)

---

### Task 1: Branch, `set_fresh` bench, pre-fix baseline

**Files:**
- Modify: `crates/segcache/benches/benchmark.rs`

- [ ] **Step 1: Create the branch**

```bash
cd /Users/brian/workspace/brayniac/cache-rs
git checkout -b fresh-key-dedup main
```

- [ ] **Step 2: Add the `set_fresh` benchmark**

In `crates/segcache/benches/benchmark.rs`, after `set_benchmark` (ends near line 110), add:

```rust
fn set_fresh_benchmark(c: &mut Criterion) {
    let ttl = Duration::ZERO;
    let mut group = c.benchmark_group("set_fresh");
    group.measurement_time(Duration::from_secs(30));
    group.throughput(Throughput::Elements(1));

    // Monotonically unique keys: every op is a genuine FRESH-key insert.
    // The `set` bench cycles a fixed 1M-key set and is mostly overwrites
    // after warmup, diluting exactly the fresh-key claim path this bench
    // isolates. hash_power 20 (1M slots) comfortably exceeds what the
    // 64MB heap holds live, so inserts exercise the claim path rather
    // than the table-full error path; eviction churn is part of the
    // steady-state miss-fill cost being measured.
    let cache = Segcache::builder()
        .hash_power(20)
        .heap_size(64 * MB)
        .segment_size(MB as i32)
        .build()
        .expect("failed to create cache");

    let value = vec![0u8; 64];
    let mut counter: u64 = 0;

    group.bench_function("8b/64b", |b| {
        b.iter(|| {
            let key = counter.to_be_bytes();
            counter += 1;
            let _ = cache.insert(&key, &value[..], None, ttl);
        })
    });
}
```

Then add `set_fresh_benchmark` to the `criterion_group!` list at the bottom of the file (keep the existing entries; insert it after `set_benchmark`'s entry).

- [ ] **Step 3: Verify the bench compiles and runs briefly**

Run: `cargo bench -p segcache -- set_fresh --profile-time 5`
Expected: compiles, runs `set_fresh/8b/64b` for ~5s, no panic.

- [ ] **Step 4: Capture the pre-fix baseline**

Quiesce the machine as much as practical (7f lesson: thermal noise moved `set` numbers). Then:

Run: `cargo bench -p segcache -- --save-baseline pre "set_fresh|set/1b/64b|get/1b/0b|hot_counter"`
Expected: ~4 benches × 30s. Record the `set_fresh`, `set/1b/64b`, `get/1b/0b`, and `incr/hot_counter` numbers in the task notes — they are the A/B denominators for Task 7.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/benches/benchmark.rs
git commit -m "bench: add set_fresh (unique-key miss-fill) benchmark"
```

---

### Task 2: `Mutex` re-export in `crate::sync`

**Files:**
- Modify: `crates/segcache/src/sync.rs`

- [ ] **Step 1: Add the re-export**

Append to `crates/segcache/src/sync.rs`:

```rust
#[cfg(not(feature = "loom"))]
pub use std::sync::Mutex;

#[cfg(feature = "loom")]
pub use loom::sync::Mutex;
```

(Both have the same `lock() -> LockResult<MutexGuard>` API; the loom variant makes the Task 5 model actually explore lock interleavings — a std Mutex inside a loom model would be invisible to loom and can deadlock its scheduler.)

- [ ] **Step 2: Verify both cfgs compile**

Run: `cargo check -p segcache && cargo check -p segcache --features loom`
Expected: both succeed (the re-export is unused until Task 4 — an unused-import warning does not occur for `pub use`).

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/sync.rs
git commit -m "segcache: re-export Mutex (std/loom) from crate::sync"
```

---

### Task 3: Split `try_link_in_bucket`; scan-all-choices-before-claim (fixes the single-threaded cross-bucket duplicate)

**Files:**
- Modify: `crates/segcache/src/hashtable/table.rs` (functions near lines 654-754 and 1061-1105; tests module near line 1195)

**Background:** today `insert()` calls `try_link_in_bucket` per bucket, and that function runs ALL THREE passes (live/ghost match → empty claim → any-ghost claim) before `insert()` moves to the next candidate bucket. So a key whose live entry sits in its second-choice bucket (because the first was full when it was inserted) gets DUPLICATED by a later insert if the first-choice bucket has since freed a slot — single-threaded, no race needed. The restructure (all-buckets live scan first, claims after) fixes this and is the skeleton the Task 4 lock brackets.

- [ ] **Step 1: Add the `count_live_entries` test helper and refactor the F4 test to use it**

In the `mod tests` block of `table.rs` (starts near line 1195), after the `MockVerifier` impl, add:

```rust
    /// Count live (non-empty, non-ghost) entries across `key`'s candidate
    /// buckets whose tag matches and whose location verifies for `key`.
    fn count_live_entries(
        ht: &MultiChoiceHashtable,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> usize {
        let hash = ht.hash_key(key);
        let tag = MultiChoiceHashtable::tag_from_hash(hash);
        let buckets = ht.bucket_indices(hash);
        let num_choices = ht.num_choices as usize;

        let mut live_count = 0;
        for &bucket_index in &buckets[..num_choices] {
            let bucket = ht.bucket(bucket_index);
            for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) {
                    continue;
                }
                if Hashbucket::tag(packed) != tag {
                    continue;
                }
                if verifier.verify(key, Hashbucket::location(packed), true) {
                    live_count += 1;
                }
            }
        }
        live_count
    }
```

Then in `test_concurrent_same_key_insert_no_duplicates`, replace the inline counting block (the `let hash = ht.hash_key(KEY);` line near 1415 through the `}` closing the outer `for` loop near 1436) with:

```rust
        let live_count = count_live_entries(&ht, KEY, &*verifier);
```

(keep the `assert_eq!` that follows it).

Run: `cargo test -p segcache test_concurrent_same_key_insert_no_duplicates`
Expected: PASS (pure refactor).

- [ ] **Step 2: Write the failing cross-bucket duplicate test**

Add to `mod tests`:

```rust
    // A live entry must be REPLACED wherever it lives among the candidate
    // buckets — never shadowed by a fresh claim in an earlier bucket.
    // Setup: fill the key's first-choice bucket so its first insert lands
    // in the second-choice bucket, then free a first-bucket slot and
    // insert the key again. The old per-bucket pass order (match/empty/
    // ghost fully in bucket 0 before looking at bucket 1) claimed the
    // freed first-bucket slot and left TWO live entries — single-threaded,
    // no race required.
    #[test]
    fn test_replace_across_buckets_no_duplicate() {
        let ht = MultiChoiceHashtable::new(7); // 16 buckets
        let mut verifier = MockVerifier::new();

        // Find a key whose two candidate buckets differ.
        let mut key: Vec<u8> = Vec::new();
        for i in 0u64.. {
            let cand = format!("xbucket-{i}").into_bytes();
            let ch = ht.bucket_indices(ht.hash_key(&cand));
            if ch[0] != ch[1] {
                key = cand;
                break;
            }
        }
        let buckets = ht.bucket_indices(ht.hash_key(&key));
        let b0 = buckets[0];

        // Brute-force 8 filler keys whose FIRST choice is b0; inserting
        // them fills b0 with live entries of OTHER keys.
        let mut fillers: Vec<Vec<u8>> = Vec::new();
        for i in 0u64.. {
            if fillers.len() == Hashbucket::NUM_ITEM_SLOTS {
                break;
            }
            let cand = format!("filler-{i}").into_bytes();
            if ht.bucket_indices(ht.hash_key(&cand))[0] == b0 {
                fillers.push(cand);
            }
        }
        for (n, f) in fillers.iter().enumerate() {
            let loc = Location::new(100 + n as u64);
            verifier.add(f, loc, false);
            assert_eq!(ht.insert(f, loc, &verifier), Ok(None));
        }

        // b0 is full -> the key's first insert lands in its second choice.
        let loc_a = Location::new(1);
        verifier.add(&key, loc_a, false);
        assert_eq!(ht.insert(&key, loc_a, &verifier), Ok(None));

        // Free one b0 slot, then insert the key again: it MUST replace
        // the second-choice entry (returning loc_a), not claim the freed
        // b0 slot alongside it.
        assert!(ht.remove(&fillers[0], Location::new(100)));
        let loc_b = Location::new(2);
        verifier.add(&key, loc_b, false);
        assert_eq!(ht.insert(&key, loc_b, &verifier), Ok(Some(loc_a)));

        assert_eq!(
            count_live_entries(&ht, &key, &verifier),
            1,
            "cross-bucket replace must not leave a duplicate"
        );
    }
```

- [ ] **Step 3: Run it — verify it fails on current code**

Run: `cargo test -p segcache test_replace_across_buckets_no_duplicate`
Expected: FAIL — the second insert returns `Ok(None)` (claimed the freed b0 slot) instead of `Ok(Some(loc_a))`, or `count_live_entries` returns 2.

- [ ] **Step 4: Add `probe_with_hash` and make `probe` delegate**

Replace `probe` (near line 178) with:

```rust
    /// Hash a key once and derive its raw hash, tag, and N-choice bucket
    /// indices. The raw hash also selects the insert stripe (see `insert`).
    #[inline]
    fn probe_with_hash(&self, key: &[u8]) -> (u64, u16, [usize; MAX_CHOICES as usize]) {
        let hash = self.hash_key(key);
        (hash, Self::tag_from_hash(hash), self.bucket_indices(hash))
    }

    /// Hash a key once and derive its tag and N-choice bucket indices.
    ///
    /// Every keyed operation starts here, so the single hash and its
    /// expansion into candidate buckets live in one place.
    #[inline]
    fn probe(&self, key: &[u8]) -> (u16, [usize; MAX_CHOICES as usize]) {
        let (_hash, tag, buckets) = self.probe_with_hash(key);
        (tag, buckets)
    }
```

- [ ] **Step 5: Replace `try_link_in_bucket` with the two split functions**

Delete `try_link_in_bucket` (lines ~647-754) entirely and put these two functions in its place. First confirm it has no other callers: `grep -n "try_link_in_bucket" crates/segcache/src/hashtable/table.rs` must show only `insert()`'s three call sites (lines ~1074, ~1089, ~1097) plus comments/tests text.

```rust
    /// Replace the key's existing LIVE entry in this bucket, if present,
    /// via a same-slot CAS retry loop (item 7f, F4: on a matching-slot CAS
    /// failure, re-read the SAME slot — a racing same-key writer's update
    /// must be seen, never skipped).
    ///
    /// Ghost slots are deliberately NOT taken here: taking over a ghost
    /// CREATES a live entry for the key, and all entry creation is
    /// serialized under the insert stripe lock (`try_claim_new_slot`).
    /// Without that split, two racing fresh inserters could each take over
    /// a different same-tag ghost (one per candidate bucket) and publish a
    /// duplicate on the lock-free path.
    ///
    /// Every successful `compare_exchange` publishes with `Release`: it is
    /// the linearization point exposing a location to readers, ordering
    /// the item bytes written by reserve/define ahead of it (concurrent-
    /// reserve spec §4).
    ///
    /// Returns `Some(old_location)` if this call replaced a live entry,
    /// `None` if this bucket holds no live entry for the key.
    fn try_replace_existing(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        new_packed: u64,
        verifier: &impl KeyVerifier,
    ) -> Option<Location> {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if Hashbucket::tag(packed) != tag || Hashbucket::is_ghost(packed) {
                    break; // not a live entry with our tag — next slot
                }

                let location = Hashbucket::location(packed);

                if !verifier.verify(key, location, true) {
                    break; // a DIFFERENT key occupies this slot — next slot
                }

                let freq = Hashbucket::freq(packed);
                let new_with_freq = Hashbucket::with_freq(new_packed, freq);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_with_freq,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(location),
                    // Re-read THIS slot — a racing same-key writer changed it.
                    Err(_) => continue,
                }
            }
        }

        None
    }

    /// Claim a NEW live entry for the key in this bucket: a matching-tag
    /// ghost first (freq-preserving takeover), then an empty slot, then
    /// any ghost. Returns true if a slot was claimed.
    ///
    /// Entry creation only — the caller (`insert`) has already established
    /// that no live entry for the key exists, and (from Task 4 on) holds
    /// the key's insert stripe lock while calling this.
    fn try_claim_new_slot(&self, bucket_index: usize, tag: u16, new_packed: u64) -> bool {
        let bucket = self.bucket(bucket_index);

        // Matching-tag ghost: take it over, preserving its frequency.
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if Hashbucket::tag(packed) != tag || !Hashbucket::is_ghost(packed) {
                    break; // next slot
                }

                let freq = Hashbucket::freq(packed);
                let new_with_freq = Hashbucket::with_freq(new_packed, freq);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_with_freq,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(_) => continue, // re-read THIS slot
                }
            }
        }

        // Empty slot.
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let packed = bucket.items[slot_index].load(Ordering::Relaxed);

            if packed == 0 {
                match bucket.items[slot_index].compare_exchange(
                    0,
                    new_packed,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(_) => continue,
                }
            }
        }

        // Any ghost (evict it).
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if Hashbucket::is_ghost(speculative) {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if Hashbucket::is_ghost(packed) {
                    match bucket.items[slot_index].compare_exchange(
                        packed,
                        new_packed,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return true,
                        Err(_) => continue,
                    }
                }
            }
        }

        false // bucket full of live entries
    }
```

- [ ] **Step 6: Restructure `insert()`**

Replace the body of `insert` (lines ~1061-1105) with:

```rust
    fn insert(
        &self,
        key: &[u8],
        location: Location,
        verifier: &impl KeyVerifier,
    ) -> Result<Option<Location>, ()> {
        let (_hash, tag, buckets) = self.probe_with_hash(key);
        let choices = &buckets[..self.num_choices as usize];

        let new_packed = Hashbucket::pack(tag, 1, location);

        // Replace the key's existing LIVE entry, wherever it lives among
        // the candidate buckets. Scanning ALL choices before any claim is
        // load-bearing: claiming a new slot in an earlier bucket while the
        // key's live entry sits in a later one would publish a duplicate.
        for &bucket_index in choices {
            if let Some(old) =
                self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
            {
                return Ok(Some(old));
            }
        }

        // Fresh key: claim a new slot (matching ghost, then empty, then
        // any ghost — per bucket, in choice order).
        for &bucket_index in choices {
            if self.try_claim_new_slot(bucket_index, tag, new_packed) {
                return Ok(None);
            }
        }

        // All candidate buckets full of live entries: retry least-full
        // first (a racing remove may have freed a slot since the scan).
        if self.num_choices > 1 {
            let mut sorted: Vec<_> = choices.to_vec();
            sorted.sort_by_key(|&b| self.count_occupied(b));
            for bucket_index in sorted {
                if self.try_claim_new_slot(bucket_index, tag, new_packed) {
                    return Ok(None);
                }
            }
        }

        Err(())
    }
```

(Note: the old "second pass" computed a `min_by_key` target AND then a sorted retry — the sorted loop's first element IS the least-full bucket, so the separate `min_by_key` probe was redundant and is dropped.)

- [ ] **Step 7: Run the new test — verify it passes**

Run: `cargo test -p segcache test_replace_across_buckets_no_duplicate`
Expected: PASS.

- [ ] **Step 8: Run the full hashtable + segcache suites**

Run: `cargo test -p segcache && cargo test -p segcache --features debug`
Expected: all PASS (behavioral parity everywhere except the fixed duplicate path).

- [ ] **Step 9: Commit**

```bash
git add crates/segcache/src/hashtable/table.rs
git commit -m "segcache: split try_link_in_bucket; replace live entries across all buckets before claiming

Fixes a latent single-threaded duplicate: insert could claim a freed
slot in the first-choice bucket while the key's live entry sat in the
second-choice bucket."
```

---

### Task 4: Striped insert lock (the concurrency fix)

**Files:**
- Modify: `crates/segcache/src/hashtable/table.rs`

- [ ] **Step 1: Write the failing concurrent fresh-key test**

Add to `mod tests` in `table.rs`:

```rust
    // The fresh-key duplicate-publish race (item 7f's tracked follow-up):
    // with NO seed entry, racing first inserts of one key could each pass
    // the live-entry scan and then claim two DIFFERENT slots (same or
    // different candidate bucket). The insert stripe lock serializes entry
    // creation with an under-lock re-check, so exactly one live entry
    // must survive every trial.
    #[test]
    fn test_concurrent_fresh_key_insert_no_duplicates() {
        use std::sync::{Arc, Barrier};

        const NUM_THREADS: usize = 4;
        const TRIALS: usize = 2000;

        for trial in 0..TRIALS {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let key = format!("fresh-{trial}").into_bytes();

            let mut verifier = MockVerifier::new();
            for t in 0..NUM_THREADS {
                verifier.add(&key, Location::new((t + 1) as u64), false);
            }
            let verifier = Arc::new(verifier);
            let barrier = Arc::new(Barrier::new(NUM_THREADS));

            std::thread::scope(|scope| {
                for t in 0..NUM_THREADS {
                    let ht = ht.clone();
                    let verifier = verifier.clone();
                    let barrier = barrier.clone();
                    let key = key.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let _ = ht.insert(&key, Location::new((t + 1) as u64), &*verifier);
                    });
                }
            });

            assert_eq!(
                count_live_entries(&ht, &key, &*verifier),
                1,
                "trial {trial}: fresh-key race published a duplicate"
            );
        }
    }
```

- [ ] **Step 2: Run it — verify it fails on the unlocked code**

Run: `cargo test -p segcache --release test_concurrent_fresh_key_insert_no_duplicates`
Expected: FAIL (some trial reports 2+ live entries). This red is probabilistic — the race was reliably observed during 7f development, but if it passes on this machine, raise `TRIALS` to 10000 and/or `NUM_THREADS` to 8 until it reliably fails, THEN proceed. Do not skip the red run: it is what makes the Task 4 Step 6 bite-check meaningful.

- [ ] **Step 3: Add the stripe field and constructor wiring**

In `table.rs`:

Change the import (line 13) from `use crate::sync::Ordering;` to:

```rust
use crate::sync::{Mutex, Ordering};
```

and add below it:

```rust
use crossbeam_utils::CachePadded;
```

Add to the `MultiChoiceHashtable` struct (after `num_choices: u8,`):

```rust
    /// Striped insert locks. Entry CREATION for a key (empty-slot claim,
    /// ghost takeover) is serialized per key-hash stripe with an
    /// under-lock absence re-check (see `insert`); entry MUTATION
    /// (replace, relocate, remove, ghost-convert) stays lock-free.
    ///
    /// LOCK: leaf — the critical section is pure bucket-word CAS +
    /// verifier reads; it is never held across any other lock, pin
    /// acquisition, or wait.
    insert_locks: Box<[CachePadded<Mutex<()>>]>,
```

Add the stripe-count const inside `impl MultiChoiceHashtable` (near the top, before `new`):

```rust
    /// Insert-lock stripe count (power of two). Contention needs two
    /// concurrent FRESH inserts whose key hashes collide mod the stripe
    /// count — rare, and a collision costs a short wait, not correctness.
    /// Under loom the array shrinks: loom tracks every sync object, and
    /// models only ever touch one key (one stripe).
    const NUM_STRIPES: usize = if cfg!(feature = "loom") { 4 } else { 1024 };
```

In `with_choices`, before the final `Self { .. }` expression, add:

```rust
        let insert_locks = (0..Self::NUM_STRIPES)
            .map(|_| CachePadded::new(Mutex::new(())))
            .collect::<Vec<_>>()
            .into_boxed_slice();
```

and add `insert_locks,` to the `Self { .. }` initializer.

Add the accessor (next to `bucket()`):

```rust
    /// The insert stripe for a key hash (see `insert_locks`).
    #[inline]
    fn stripe(&self, hash: u64) -> &Mutex<()> {
        &self.insert_locks[(hash as usize) & (Self::NUM_STRIPES - 1)]
    }
```

- [ ] **Step 4: Take the lock on the fresh-key path**

In the restructured `insert()` (Task 3 Step 6), change `let (_hash, ...)` to `let (hash, ...)`, and insert between the replace loop and the claim loop:

```rust
        // Fresh key: entry CREATION is serialized per key-hash stripe.
        // Two racing fresh inserters of one key both reach here; the
        // loser of the lock sees the winner's entry in the re-check below
        // and resolves to a replace. Mutation paths never make an
        // existing key's entry vanish-and-reappear (replace/relocate are
        // in-place slot CASes; a concurrent delete linearizes as
        // delete-then-insert), so a re-check miss really means absent.
        // The stripe lock is a LEAF: the critical section is pure
        // bucket-word CAS + verifier reads — it never takes another lock,
        // pin, or wait.
        let _guard = self.stripe(hash).lock().unwrap();

        // Re-check under the lock: a racing fresh insert may have
        // published while we waited.
        for &bucket_index in choices {
            if let Some(old) =
                self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
            {
                return Ok(Some(old));
            }
        }
```

(The claim loops and the least-full retry that follow now run under `_guard`; the early `return`s on the replace paths drop it automatically.)

- [ ] **Step 5: Run the concurrent test — verify it passes**

Run: `cargo test -p segcache --release test_concurrent_fresh_key_insert_no_duplicates`
Expected: PASS, all trials.

- [ ] **Step 6: Bite-check the lock**

Temporarily comment out the `let _guard = ...` line (re-edit only — never `git checkout`), rerun Step 5's command: expected FAIL. Restore the line by re-editing, rerun: expected PASS.

- [ ] **Step 7: Full suite + lints**

Run: `cargo test -p segcache && cargo test -p segcache --features debug && cargo clippy -p segcache --all-targets -- -D warnings && cargo fmt --all --check`
Expected: all clean.

- [ ] **Step 8: Commit**

```bash
git add crates/segcache/src/hashtable/table.rs
git commit -m "segcache: serialize hashtable entry creation with striped insert locks

Closes the fresh-key duplicate-publish race (7f follow-up): all entry
creation (empty-slot claim, ghost takeover) happens under the key's
stripe with an under-lock absence re-check; overwrites stay lock-free."
```

---

### Task 5: Loom model

**Files:**
- Modify: `crates/segcache/src/hashtable/table.rs` (`mod loom_tests`, near line 1447)

- [ ] **Step 1: Add the model**

Add to `mod loom_tests`:

```rust
    // Fresh-key insert de-dup: two threads race the very first insert of
    // one key; the stripe lock (loom::sync::Mutex under this cfg)
    // serializes entry creation, so exactly one live entry may exist
    // post-join. A mutex-serialized invariant is SC-independent, so —
    // unlike the SeqCst Dekker pairs — loom genuinely verifies this one.
    #[test]
    fn loom_fresh_key_insert_single_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || {
                let _ = ht1.insert(b"key", Location::new(1), &*v1);
            });

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || {
                let _ = ht2.insert(b"key", Location::new(2), &*v2);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            // Count live same-tag entries across the key's candidate
            // buckets (AlwaysVerifier verifies anything, so tag-match
            // suffices — only this one key was ever inserted).
            let hash = ht.hash_key(b"key");
            let tag = MultiChoiceHashtable::tag_from_hash(hash);
            let buckets = ht.bucket_indices(hash);
            let mut live = 0;
            for &bucket_index in &buckets[..ht.num_choices as usize] {
                let bucket = ht.bucket(bucket_index);
                for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
                    let packed = bucket.items[slot_index].load(Ordering::Acquire);
                    if packed != 0
                        && !Hashbucket::is_ghost(packed)
                        && Hashbucket::tag(packed) == tag
                    {
                        live += 1;
                    }
                }
            }
            assert_eq!(
                live, 1,
                "fresh-key race must resolve to exactly one live entry"
            );
        });
    }
```

- [ ] **Step 2: Run the full loom suite**

Run: `cargo test -p segcache --features loom -- loom`
Expected: all models PASS — the 19 existing plus this one (20 total). If the new model is slow (loom explores lock interleavings too), that is acceptable; if it exceeds a few minutes, add the same `loom::model::Builder` preemption-bound pattern used by the models near line 1662.

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/hashtable/table.rs
git commit -m "segcache: loom model for fresh-key insert single-entry invariant"
```

---

### Task 6: Segcache-level resurrection test + comment reconciliation

**Files:**
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs`
- Modify: `crates/segcache/src/segcache.rs` (comments only)
- Modify: `crates/segcache/src/hashtable/table.rs` (comments only)

- [ ] **Step 1: Add the resurrection stress test**

In `eviction_concurrency_tests.rs`, after the existing tests, add (match the file's existing test style — it drives the public `&self` API):

```rust
/// Fresh-key insert de-dup (the follow-up the module note above used to
/// scope OUT — now fixed by the hashtable's striped insert locks):
/// threads race the FIRST insert of a brand-new key — deliberately NO
/// seeding — then the key is deleted once. Before the fix, racing fresh
/// inserts could publish TWO live hashtable entries; `delete` unlinked
/// only the first, so the key RESURRECTED with the losing insert's value.
/// Post-fix: after one delete the key must be gone, every trial.
#[test]
fn concurrent_fresh_insert_no_resurrection() {
    use std::sync::{Arc, Barrier};

    const THREADS: usize = 4;
    const TRIALS: usize = 1000;

    let cache = Segcache::builder()
        .segment_size(64 * 1024)
        .heap_size(8 * 1024 * 1024)
        .hash_power(13)
        .build()
        .expect("failed to build cache");
    let cache = Arc::new(cache);

    for trial in 0..TRIALS {
        let key = format!("fresh-{trial:06}");
        let barrier = Arc::new(Barrier::new(THREADS));

        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let cache = cache.clone();
                let barrier = barrier.clone();
                let key = key.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let value = format!("V{t:02}{trial:06}");
                    let _ = cache.insert(
                        key.as_bytes(),
                        value.as_bytes(),
                        None,
                        std::time::Duration::ZERO,
                    );
                });
            }
        });

        // Exactly one live entry means ONE delete fully removes the key.
        // (No assert on delete's return: eviction pressure could
        // legitimately have dropped the key already — the invariant under
        // test is only "never resurrected".)
        let _ = cache.delete(key.as_bytes());
        assert!(
            cache.get(key.as_bytes()).is_none(),
            "trial {trial}: key resurrected after delete — duplicate entry"
        );
    }
}
```

- [ ] **Step 2: Run it (release, like the other stress tests)**

Run: `cargo test -p segcache --release concurrent_fresh_insert_no_resurrection`
Expected: PASS. (Optional sanity: this test also fails pre-fix — it was covered by Task 4's bite-check at the hashtable layer, so no second bite-check is required here.)

- [ ] **Step 3: Reconcile stale comments**

Three sites, comments only — no code changes:

1. `eviction_concurrency_tests.rs` module note (the paragraph near line 1580 beginning "Every test SEEDS its shared key(s) single-threaded..."): keep the seeding rationale (determinism + freq>0 merge survival) but replace the "separate, already-tracked... window" framing. Rewrite the second half of the paragraph to:

```
// Every test below still SEEDS its shared key(s) single-threaded before
// spawning any thread, so every concurrent op in the storm is an
// OVERWRITE (or a delete) of an ALREADY-PUBLISHED key. The seeding is
// kept for determinism (freq > 0 merge survival, exact-count asserts),
// not out of necessity: concurrent FRESH-key inserts are de-duplicated
// by the hashtable's striped insert locks (see `table.rs::insert` and
// `concurrent_fresh_insert_no_resurrection` below).
```

2. `table.rs`, the F4 test's doc comment: the `NOTE:` paragraph (near line 1356, "a separate, pre-existing race was observed... flagged for a follow-up item") — replace with:

```
    // NOTE: the fresh-key half of this race (multiple threads racing the
    // very FIRST insert of a key, each claiming a different empty slot
    // across the first-pass/second-pass TOCTOU) is closed by the striped
    // insert locks — `test_concurrent_fresh_key_insert_no_duplicates`
    // below covers it.
```

3. `segcache.rs`, the fresh-key arm comment (lines ~288-303, "Fresh key: insert. ..."): rewrite to reflect the new hashtable contract:

```rust
                    // Fresh key: `hashtable.insert()` is an atomic upsert
                    // whose entry CREATION is serialized per key-hash
                    // stripe (table.rs), so concurrent fresh inserts of
                    // one key can never publish duplicate entries. If a
                    // racing writer published this key between our
                    // `lookup_slot` miss and here, our call resolves to a
                    // replace under the stripe's re-check and returns the
                    // racer's location as `Ok(Some(raced_old))` — that
                    // racer's segment accounting is then ours to
                    // decrement, with the unlink already done by the call
                    // above rather than by a pin-first `cas_location` (a
                    // narrow, accepted gap: if a drain claims that
                    // segment between the unlink and the pin attempt
                    // below, the pin fails and the drain owns the
                    // segment's accounting wholesale).
```

- [ ] **Step 4: Full suite**

Run: `cargo test -p segcache && cargo test -p segcache --features debug && cargo fmt --all --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs crates/segcache/src/segcache.rs crates/segcache/src/hashtable/table.rs
git commit -m "segcache: fresh-insert resurrection stress test; reconcile follow-up comments"
```

---

### Task 7: Full verification + benchmark A/B + gate report

**Files:** none (verification only)

- [ ] **Step 1: Full CI-equivalent verification**

Run each; all must be clean:

```bash
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --release
cargo test -p segcache --features loom -- loom
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```

- [ ] **Step 2: Benchmark A/B against the Task 1 baseline**

Same machine, quiesced as in Task 1:

Run: `cargo bench -p segcache -- --baseline pre "set_fresh|set/1b/64b|get/1b/0b|hot_counter"`
Expected: criterion prints per-bench change vs the `pre` baseline.

- [ ] **Step 3: Report the gate numbers to Brian**

Report: `set_fresh` delta (the fix's cost), `set/1b/64b`, `get/1b/0b`, `incr/hot_counter` deltas (sentinels — should be noise). Frame against the spec's gate: working prior is ~5-10ns added to fresh inserts only; `set_fresh` regression beyond ~10% or real movement in `set` puts Phase 2 (lock-free claim-then-resolve, spec §5) on the table. **Brian makes the call** — do not proceed to a PR or to Phase 2 without his verdict.

- [ ] **Step 4 (after Brian approves the numbers): finish the branch**

Use superpowers:finishing-a-development-branch — self-review via the pr-adversarial-review skill, then PR per this repo's convention (PRs to `main`, title style `segcache: <what> (<roadmap ref>)`).

---

## Self-review notes

- Spec §3 (structure/protocol) → Tasks 2-4. Spec §4 (cleanups) → Task 6. Spec §6 tests → Tasks 3-6; bench gate → Tasks 1 + 7. Spec §5 (Phase 2) deliberately unplanned — contingent on the Task 7 gate.
- The Task 3 restructure changes one observable priority: a live entry in a later candidate bucket now beats ghost-takeover/empty-claim in an earlier one (that was the duplicate bug); per-bucket creation order (matching ghost → empty → any ghost) and cross-bucket choice order are preserved.
- Type consistency: `try_replace_existing -> Option<Location>`, `try_claim_new_slot -> bool`, `probe_with_hash -> (u64, u16, [usize; MAX_CHOICES as usize])`, `stripe -> &Mutex<()>` — used consistently across Tasks 3-5.
