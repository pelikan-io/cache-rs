# Drain-Safe Merge Eviction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Merge policy's in-place `Segment::compact()` with a reader-safe copy-to-spare protocol, so no eviction path mutates a readable segment's bytes in place (roadmap item 5b).

**Architecture:** Merge copies surviving items out of each candidate into a fresh **spare** segment (reserved from a held-back spare queue), publishing each via the hashtable's existing Release-CAS relink, then drains the candidate via the existing `clear_segment`/condemn machinery. This mirrors the already-proven `s3fifo_evict_admission`/`_main` paths. A cheap whole-segment expiration is attempted first. `compact()` is deleted. Eviction stays `&mut`-serialized; this is machinery for item 7.

**Tech Stack:** Rust, `crate::sync` atomics, crossbeam-deque `Injector` (the existing free queue), criterion benches, `std::thread::scope` for the reader-safety stress test.

**Spec:** `docs/superpowers/specs/2026-07-17-drain-safe-merge-design.md`

**Branch:** `drain-safe-merge` (already created; the spec commit is on it).

**Conventions:**
- New files get NO license header (Pelikan-only policy).
- All commits end with:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017xPi3BW7qJUxXxX9Pcjm7w
  ```
- Run commands from repo root `/Users/brian/workspace/brayniac/cache-rs`.
- During bite-checks (temporarily breaking code to prove a test fails), restore by re-editing — NEVER `git checkout <file>`.
- CI enforces `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all --check`.

**Key existing primitives (verify signatures as you go):**
- `Segments::reserve_free(&self) -> Option<NonZeroU32>` — returns a `Reserved` segment from the free queue (statistics reset, generation bumped).
- `Segments::recycle(&mut self, id)` — unlinks a `Draining` segment and returns it to the free queue.
- `Segments::condemn(&mut self, id, next, prev) -> ClearOutcome` — race-fix free-path pushes to the free queue at segments.rs:609.
- `Segments::clear_segment(&mut self, id, hashtable, expire) -> Result<ClearOutcome, ()>` — `Sealed→Draining` + ref_count recheck + condemn-if-pinned; on unpinned it calls `recycle`.
- `Segments::link_at_head(&mut self, this, head)` — links a `Reserved` segment at the front of a chain, publishes it `Sealed`.
- `Segment::prune(&mut self, hashtable, cutoff_freq, target_ratio) -> f64` — marks low-frequency items deleted, returns adjusted cutoff.
- `Segment::copy_into(&mut self, target: &mut Segment, hashtable) -> Result<(), SegmentsError>` — appends this segment's live survivors past `target`'s write-offset, each relinked by `cas_location` (Release publish). **This is the reader-safe copy the rework reuses.**
- `Segment::compact(&mut self, hashtable)` — the in-place relocation being **deleted**.
- `SegmentHeader::try_reserve(&self) -> bool`, `try_release(&self) -> bool`.
- `TtlBucket::head()`, `set_head(Option<NonZeroU32>)`, `next_to_merge()`, `set_next_to_merge(...)`.
- Metrics in scope: `SEGMENT_FREE` (gauge), `SEGMENT_RETURN`, `SEGMENT_REQUEST`, `SEGMENT_REQUEST_SUCCESS`, `SEGMENT_MERGE`, `SEGMENT_PINNED_SKIP`.

---

### Task 1: Spare-queue infrastructure in `Segments`

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs`
- Test: same file (unit tests module, or `crates/segcache/src/tests.rs` — match where segment unit tests already live)

Add a held-back spare queue so merge always has a copy destination at "full". `reserve_free` (normal writes) must NOT touch it.

- [ ] **Step 1: Write the failing tests**

Add to the segments test module (find the existing `#[cfg(test)] mod tests` in `segments.rs` or the relevant tests file; if none, add `#[cfg(all(test, not(feature = "loom")))] mod spare_tests` at the bottom of `segments.rs`):

```rust
#[cfg(all(test, not(feature = "loom")))]
mod spare_tests {
    use super::*;
    use crate::eviction::Policy;

    fn build(policy: Policy, segs: usize) -> Segments {
        SegmentsBuilder::default()
            .segment_size(4096)
            .heap_size(4096 * segs)
            .eviction_policy(policy)
            .build()
            .expect("build segments")
    }

    #[test]
    fn merge_policy_holds_back_one_spare() {
        let segments = build(Policy::Merge { max_merge: 8, n_merge: 4, target_ratio: 0.25 }, 16);
        // 16 total: 1 spare + 15 free.
        assert_eq!(segments.spare_capacity(), 1);
        assert_eq!(segments.free(), 16, "free() counts free + spare");
        assert_eq!(segments.free_only(), 15, "normal free queue excludes the spare");
    }

    #[test]
    fn non_merge_policy_holds_back_no_spare() {
        let segments = build(Policy::Random, 16);
        assert_eq!(segments.spare_capacity(), 0);
        assert_eq!(segments.free_only(), 16);
    }

    #[test]
    fn reserve_spare_prefers_spare_then_falls_back_to_free() {
        let mut segments = build(Policy::Merge { max_merge: 8, n_merge: 4, target_ratio: 0.25 }, 4);
        // Drain the whole normal free queue via reserve_free (3 segments).
        let mut taken = Vec::new();
        while let Some(id) = segments.reserve_free() {
            taken.push(id);
        }
        assert_eq!(taken.len(), 3, "reserve_free must not hand out the spare");
        // The spare is still available to reserve_spare.
        let spare = segments.reserve_spare().expect("spare available at full");
        // Now truly empty.
        assert!(segments.reserve_spare().is_none());
        // Returning the spare replenishes the spare queue first.
        segments.release_unused(spare);
        assert_eq!(segments.spare_count(), 1, "return replenished the spare, not the free queue");
    }
}
```

(Adjust the `Policy::Merge { .. }` field names to the real `Policy` enum — check `crates/segcache/src/eviction/policy.rs`.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p segcache spare_tests -- --nocapture`
Expected: FAIL to compile (`spare_capacity`, `free_only`, `spare_count`, `reserve_spare` don't exist).

- [ ] **Step 3: Add the fields**

In the `Segments` struct (segments.rs ~line 17), after `free_queue`:

```rust
    /// Held-back spare segments for merge compaction. Never handed out by
    /// `reserve_free` (normal writes), so a destination is always available
    /// to merge even when the main free queue is empty.
    spare_queue: Box<crossbeam_deque::Injector<u32>>,
    /// Target number of segments to keep in the spare queue.
    spare_capacity: u32,
    /// Current spare-queue depth (atomic for item-7 readiness).
    spare_count: crate::sync::AtomicU32,
```

- [ ] **Step 4: Seed the queues at construction**

In `from_builder` (segments.rs ~line 56): compute `spare_capacity` from the policy and split the segments between the two queues.

```rust
        let spare_capacity: u32 = if matches!(evict_policy, Policy::Merge { .. }) { 1 } else { 0 };
```

Replace the free-queue fill loop so the first `spare_capacity` segments (by id) seed the spare queue and the rest seed the free queue:

```rust
        let free_queue = Box::new(crossbeam_deque::Injector::new());
        let spare_queue = Box::new(crossbeam_deque::Injector::new());
        for idx in 0..segments {
            let begin = segment_size as usize * idx;
            let end = begin + segment_size as usize;
            let mut segment = Segment::from_raw_parts(&headers[idx], &mut data[begin..end]);
            segment.init();

            let id = idx as u32 + 1; // segments are 1-indexed
            if (idx as u32) < spare_capacity {
                spare_queue.push(id);
            } else {
                free_queue.push(id);
            }
        }
```

Add to the returned `Self { .. }`:
```rust
            spare_queue,
            spare_capacity,
            spare_count: crate::sync::AtomicU32::new(spare_capacity),
```

Keep the `SEGMENT_FREE.set(segments as _)` metric (it counts free + spare = all segments).

- [ ] **Step 5: Add `reserve_spare`, `return_segment`, and accessors**

Next to `reserve_free` (segments.rs ~line 461):

```rust
    /// Reserve a segment for merge compaction. Prefers the held-back spare
    /// queue; falls back to the normal free queue when the spare is empty.
    /// Returns a `Reserved` segment, like `reserve_free`.
    pub(crate) fn reserve_spare(&self) -> Option<NonZeroU32> {
        loop {
            match self.spare_queue.steal() {
                crossbeam_deque::Steal::Retry => continue,
                crossbeam_deque::Steal::Empty => return self.reserve_free(),
                crossbeam_deque::Steal::Success(raw) => {
                    debug_assert!(raw >= 1 && raw <= self.cap);
                    let id = match NonZeroU32::new(raw) {
                        Some(id) => id,
                        None => return None,
                    };
                    if self.headers[raw as usize - 1].try_reserve() {
                        self.spare_count.fetch_sub(1, Ordering::Relaxed);
                        #[cfg(feature = "metrics")]
                        {
                            SEGMENT_REQUEST.increment();
                            SEGMENT_REQUEST_SUCCESS.increment();
                            SEGMENT_FREE.decrement();
                        }
                        return Some(id);
                    }
                    self.spare_queue.push(raw);
                    return None;
                }
            }
        }
    }

    /// Return a segment id to the pool, replenishing the spare queue before
    /// the free queue. The segment must already be transitioned to Free (or
    /// about to be); callers push the id after their state transition.
    fn return_segment(&self, id: u32) {
        if self.spare_count.load(Ordering::Relaxed) < self.spare_capacity {
            self.spare_count.fetch_add(1, Ordering::Relaxed);
            self.spare_queue.push(id);
        } else {
            self.free_queue.push(id);
        }
    }

    /// Total available segments (free queue + spare queue).
    #[cfg(test)]
    pub(crate) fn free(&self) -> usize {
        self.free_queue.len() + self.spare_queue.len()
    }

    /// Segments available to normal writes (free queue only).
    #[cfg(test)]
    pub(crate) fn free_only(&self) -> usize {
        self.free_queue.len()
    }

    #[cfg(test)]
    pub(crate) fn spare_capacity(&self) -> u32 {
        self.spare_capacity
    }

    #[cfg(test)]
    pub(crate) fn spare_count(&self) -> u32 {
        self.spare_count.load(Ordering::Relaxed)
    }
```

Note: there is an existing `#[cfg(test)] fn free(&self)` at segments.rs:168 that returns `self.free_queue.len()`. **Replace** it with the free+spare version above (and add `free_only`); update any test that called `free()` expecting free-queue-only semantics to use `free_only()` — grep `\.free()` in tests to check.

- [ ] **Step 6: Route `recycle` and `condemn` through `return_segment`**

In `recycle` (segments.rs:449), replace `self.free_queue.push(id.get());` with `self.return_segment(id.get());`.

In `condemn`'s race-fix free-path (segments.rs:609), replace `self.free_queue.push(id.get());` with `self.return_segment(id.get());`.

Leave `release_unused` (segments.rs:497) using the spare-aware path too: replace its `self.free_queue.push(id.get());` with `self.return_segment(id.get());` — this is what the Task-1 test `reserve_spare_prefers_spare_then_falls_back_to_free` asserts.

Do NOT change the `SegmentGuard::drop` free-path (guard.rs:62) — it holds a raw pointer to the free queue only. A candidate freed via the AwaitingRelease guard-drop handoff goes to the free queue directly; the spare self-heals on the next unpinned `recycle`. Add a one-line comment at guard.rs:62 noting this is intentional.

- [ ] **Step 7: Run tests + battery**

Run: `cargo test -p segcache spare_tests`
Expected: PASS (3 tests)

Run: `cargo test -p segcache && cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --all --check`
Expected: all pass/clean. (Existing tests still pass — the spare only changes capacity for the Merge policy, and merge isn't reworked yet, so no eviction test should regress. If a merge test that counts free segments now sees one fewer, that is the expected 1-segment holdback — adjust the test's expected count and note it.)

- [ ] **Step 8: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/segments/guard.rs
git commit -m "Add held-back spare segment queue for merge compaction"
```
(with the standard footer)

---

### Task 2: Expire-first at the `evict()` entry

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`evict`)
- Test: `crates/segcache/src/tests.rs` or integration test

Attempt cheap whole-segment expiration before the spare-consuming merge.

- [ ] **Step 1: Write the failing test**

Add a test that fills a Merge-policy cache with short-TTL items, advances time past the TTL, then triggers an insert that would need eviction, and asserts the freed segment came from expiration (no merge ran). Use the existing time control (`clocksource::coarse` — check how `ttl_buckets/tests.rs` or `integration_eviction.rs` advance the coarse clock; they likely use a test hook). If there is no time-advance hook, assert instead that after `expire()` a subsequent full-pool insert succeeds without incrementing `SEGMENT_MERGE` — capture the metric before/after. Model the test on existing `integration_eviction.rs` cases.

Concretely (adapt to the real time hook):

```rust
#[test]
fn evict_expires_before_merging() {
    // Merge-policy cache, fill it, expire everything, then one more insert
    // must succeed by reclaiming an expired segment — not by merging.
    // Assert SEGMENT_MERGE did not increment across the reclaiming insert.
}
```

- [ ] **Step 2: Run to verify failure** (or, if the behavior already partially holds, verify the test captures the intended ordering).

Run: `cargo test -p segcache evict_expires_before_merging`
Expected: FAIL (merge runs before expiration today).

- [ ] **Step 3: Add expire-first to `evict()`**

At the top of `evict` (segments.rs:626), before the policy `match`, attempt expiration and return on success:

```rust
    pub fn evict(
        &mut self,
        ttl_buckets: &mut TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Cheap path first: drop whole expired segments (no spare, no copy).
        // If any segment frees, a reserve_free will now succeed.
        if ttl_buckets.expire(hashtable, self) > 0 {
            return Ok(());
        }

        #[cfg(feature = "metrics")]
        let now = Instant::now();
        // ... existing match ...
```

Verify `TtlBuckets::expire(&mut self, &MultiChoiceHashtable, &mut Segments) -> usize` signature (ttl_buckets.rs:86); it returns the number of segments freed. Borrow check: `evict` takes `&mut self` (Segments) and `ttl_buckets: &mut TtlBuckets`; `expire` needs `&mut Segments` and `&mut TtlBuckets` — call as `ttl_buckets.expire(hashtable, self)`.

- [ ] **Step 4: Run test + battery**

Run: `cargo test -p segcache evict_expires_before_merging && cargo test -p segcache`
Expected: PASS. Watch `integration_eviction.rs` — expire-first must not break cases that rely on merge running; if a test filled with non-expiring items, expiration frees nothing and merge runs as before.

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --all --check`

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/tests.rs
git commit -m "Attempt whole-segment expiration before merge eviction"
```
(with the standard footer)

---

### Task 3: Rework `merge_evict` to copy-to-spare

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`merge_evict`)

Replace in-place `compact()` of the first candidate with copy-to-spare: reserve a spare, head-insert it, copy every candidate's survivors into it, drain every candidate.

- [ ] **Step 1: Read the current `merge_evict`** (segments.rs:944-1092) and `s3fifo_evict_admission` (segments.rs:1279-1331) as the template. The new structure mirrors s3fifo (head-insert a fresh destination, then drain sources) but loops over the candidate chain and prunes with the adaptive cutoff.

- [ ] **Step 2: Rewrite `merge_evict`**

Replace the body with the copy-to-spare structure. Keep the adaptive-cutoff / stop-bytes / max-merge parameters exactly as they are today; only the destination and the drain change.

```rust
    fn merge_evict(
        &mut self,
        start: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        #[cfg(feature = "metrics")]
        SEGMENT_MERGE.increment();

        let chain_len = self.merge_evict_chain_len(start);
        if chain_len < 3 {
            return Err(SegmentsError::NoEvictableSegments);
        }

        // Reserve the copy destination. At "full" this comes from the
        // held-back spare; if even that is empty, degrade gracefully to a
        // whole-segment drop rather than compacting a readable segment in
        // place.
        let spare_id = match self.reserve_spare() {
            Some(id) => id,
            None => return self.merge_evict_fallback_drop(start, hashtable),
        };

        // Configure and head-insert the spare as Sealed (readable,
        // evictable, never the write tail), like s3fifo's target.
        let src_ttl = self.headers[start.get() as usize - 1].ttl();
        {
            let sidx = spare_id.get() as usize - 1;
            self.headers[sidx].set_ttl(src_ttl);
            self.headers[sidx].set_pool(SegmentPool::Main);
        }
        // Bucket lookup: the caller (evict) holds &mut TtlBuckets. merge_evict
        // does not currently take ttl_buckets — thread it in (see Step 3).
        // Head-insert:
        //   let old_head = ttl_bucket.head();
        //   self.link_at_head(spare_id, old_head);
        //   ttl_bucket.set_head(Some(spare_id));

        // Adaptive threshold state (unchanged).
        let mut cutoff = 1.0;
        let mut merged = 0;
        let max_merge = self.evict.max_merge();
        let n_merge = self.evict.n_merge();
        let stop_ratio = self.evict.stop_ratio();
        let stop_bytes = (stop_ratio * self.segment_size() as f64) as i32;
        let target_ratio = if chain_len < n_merge {
            1.0 / chain_len as f64
        } else {
            self.evict.target_ratio()
        };

        let mut next_id = Some(start);
        while let Some(cand_id) = next_id {
            if merged > max_merge {
                break;
            }
            // Stop if the spare is full.
            if self.headers[spare_id.get() as usize - 1].live_bytes() >= stop_bytes {
                break;
            }
            if !self.get_mut(cand_id).map(|s| s.can_evict()).unwrap_or(false) {
                break;
            }

            // Advance the chain pointer before draining the candidate.
            next_id = self.headers[cand_id.get() as usize - 1].next_seg();

            // Prune, then copy survivors into the spare.
            {
                let mut cand = self.get_mut(cand_id)?;
                cutoff = cand.prune(hashtable, cutoff, target_ratio);
            }
            {
                let (mut cand, mut spare) = self.get_mut_pair(cand_id, spare_id)?;
                let _ = cand.copy_into(&mut spare, hashtable);
            }

            // Drain the candidate (Sealed→Draining + ref_count recheck +
            // condemn-if-pinned). clear_segment recycles unpinned candidates
            // (return_segment replenishes the spare) and condemns pinned ones;
            // its internal unlink patches the candidate's neighbours, healing
            // the chain around the still-head spare. No bucket-head fixup is
            // needed here — see the invariant note below.
            match self.clear_segment(cand_id, hashtable, false) {
                Ok(_outcome) => {}
                Err(()) => break,
            }
            merged += 1;
        }

        // next_to_merge advances to where the chain continues.
        Ok(next_id)
    }
```

**Chain-head invariant (important):** the spare is head-inserted once
(`set_head(Some(spare_id))`) and is never drained, so the bucket head always
points at the spare throughout the candidate loop. Draining a candidate only
unlinks it from the middle of `spare → cand → … → rest` (its neighbours are
patched by `clear_segment`'s `recycle`/`condemn` `unlink`), so **no
per-candidate bucket-head fixup is required** — unlike the fallback-drop path
(no spare inserted), which does fix the head. Do not add a `meta.prev.is_none()`
head fixup inside the candidate loop; it would fight the spare-is-head
invariant.

This skeleton flags for Step 3: (a) `merge_evict` needs the `TtlBucket` for the
one-time head-insert — thread `ttl_bucket: &mut TtlBucket` through; (b) the
`merge_evict_fallback_drop` helper.

- [ ] **Step 3: Thread the bucket through and add the fallback**

`evict`'s Merge arm (segments.rs:635-681) already holds `ttl_bucket = &mut ttl_buckets.buckets[bucket_id]` and calls `self.merge_evict(start, hashtable)`. Change `merge_evict`'s signature to accept the bucket, and move the head-insert / head-fixup lines into it:

```rust
    fn merge_evict(
        &mut self,
        start: NonZeroU32,
        ttl_bucket: &mut TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> { ... }
```

Fill in the one-time head-insert (`let old_head = ttl_bucket.head(); self.link_at_head(spare_id, old_head); ttl_bucket.set_head(Some(spare_id));`). Per the chain-head invariant above, there is **no** per-candidate head fixup.

Note the borrow: `evict` holds `let ttl_bucket = &mut ttl_buckets.buckets[bucket_id];` then calls `self.merge_evict(start, ttl_bucket, hashtable)`. `self` is `&mut Segments` and `ttl_bucket` is `&mut TtlBucket` from a different owner (`ttl_buckets`), so the two mutable borrows don't conflict.

Add the fallback helper — drop the head candidate whole (this frees a segment, which the caller's retry loop uses; it also replenishes the spare via return_segment):

```rust
    /// Graceful degradation when no spare is available: drop the chain head
    /// whole via the drain machinery, freeing one segment.
    fn merge_evict_fallback_drop(
        &mut self,
        start: NonZeroU32,
        ttl_bucket: &mut TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        let meta = self.headers[start.get() as usize - 1].metadata(Ordering::Acquire);
        let next = meta.next;
        match self.clear_segment(start, hashtable, false) {
            Ok(ClearOutcome::Freed) => {
                if meta.prev.is_none() {
                    ttl_bucket.set_head(next);
                }
                Ok(next)
            }
            _ => Err(SegmentsError::NoEvictableSegments),
        }
    }
```

- [ ] **Step 4: Update the `evict` Merge-arm call site**

In `evict` (segments.rs ~653), change `self.merge_evict(start, hashtable)` to `self.merge_evict(start, ttl_bucket, hashtable)`. The `set_next_to_merge(next_to_merge)` handling stays.

- [ ] **Step 5: Run the merge tests**

Run: `cargo test -p segcache` and `cargo test -p segcache --features debug`
Expected: PASS — `integration_eviction.rs` merge cases and any merge unit tests. If a test asserted exact post-merge segment counts, the copy-to-spare path frees the same net segments (N candidates drained, 1 spare consumed then replenished); adjust only if an assertion encoded the in-place-compaction intermediate state, and note why.

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --all --check`

- [ ] **Step 6: Commit**

```bash
git add crates/segcache/src/segments/segments.rs
git commit -m "Rework merge_evict to copy-to-spare (reader-safe, no in-place compaction)"
```
(with the standard footer)

---

### Task 4: Rework `merge_compact` and delete `Segment::compact()`

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`merge_compact`, its call site)
- Modify: `crates/segcache/src/segments/segment.rs` (delete `compact`)

- [ ] **Step 1: Rewrite `merge_compact`** (segments.rs:1096) the same way as `merge_evict`, minus pruning (merge_compact combines segments without dropping by frequency). Reserve a spare, head-insert it, `copy_into` each candidate into the spare (no `prune` call), `clear_segment` each candidate. Thread `ttl_bucket: &mut TtlBucket` through, add the same fallback-drop on no-spare (reuse `merge_evict_fallback_drop`). Keep the `chain_len < 2` guard and `stop_bytes`/`max_merge` stops.

Find `merge_compact`'s caller: `grep -n "merge_compact" crates/segcache/src/segments/segments.rs`. It is called at segments.rs:861 inside the S3-FIFO or a policy path — update that call site to pass the bucket, mirroring Task 3 Step 4. If the caller doesn't have a `&mut TtlBucket` in scope, obtain it the same way the Merge arm does (`ttl_buckets.get_mut_bucket(ttl)` or index by bucket id).

- [ ] **Step 2: Delete `Segment::compact`**

In `segment.rs`, delete the `compact` function (starts at segment.rs:251) and its doc comment. Verify no remaining callers: `grep -rn "\.compact(\|fn compact" crates/segcache/src` should return nothing. If `compact` used helpers that are now dead (check for a private helper only it called), delete those too (clippy will flag them).

- [ ] **Step 3: Delete the `SCOPE:` compaction comments**

The two `// SCOPE: ... in-place compaction ... roadmap item 5` comments (segments.rs:987 and :1132, the merge_evict/merge_compact compaction notes) are now obsolete — remove them (the code they described is gone).

- [ ] **Step 4: Run + battery**

Run: `cargo test -p segcache && cargo test -p segcache --features debug`
Expected: PASS. `grep -rn "compact" crates/segcache/src` — only `merge_compact`, `n_compact`/`compact_ratio` (eviction metrics), and `ITEM_COMPACTED` should remain; no `Segment::compact`.

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --all --check`

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/segments/segment.rs
git commit -m "Rework merge_compact to copy-to-spare; delete in-place Segment::compact"
```
(with the standard footer)

---

### Task 5: Reader-safety stress test + bite-check

**Files:**
- Create: `crates/segcache/src/segments/eviction_concurrency_tests.rs`
- Modify: `crates/segcache/src/segments/mod.rs` (register the module)

This is the test that would have caught in-place `compact()` moving bytes under a pinned reader. It exercises the reserve/read path (`&self`) concurrently with `&mut` eviction — which the current API keeps single-threaded, so the test drives the internal methods directly, like item 4's concurrency tests.

- [ ] **Step 1: Write the stress test** (no license header)

The evictor needs `&mut Segments`; readers need `&Segments` (`acquire_item_at` is `&self`). To run them together in one process without unsafe aliasing of `&mut`, structure the test so the evictor and readers interleave through a shared `Segments` behind the crate's existing pattern — mirror how `ttl_buckets/concurrency_tests.rs` shares `&Segments` for reserve, and drive eviction from the same thread as a controlled interleave OR use a short critical section. Simplest sound design: single evictor thread owns `&mut Segments` and performs merges in a loop; reader threads hold `&Segments` and repeatedly `acquire_item_at` + verify bytes. Since `&mut` and `&` cannot coexist safely in Rust, gate them: put `Segments` in a `std::sync::RwLock` for the test only — readers take read locks (`acquire_item_at` under a read guard, then hold the returned `SegmentGuard` across the byte check), the evictor takes the write lock per merge. This proves the RAII pin protocol: a reader that acquired an item before a merge must still read intact bytes after, because the pin defers the segment's free (AwaitingRelease), even though the merge completed under the write lock.

```rust
//! Reader-vs-eviction safety tests for drain-safe merge.

use crate::*;
use crate::eviction::Policy;
use std::sync::RwLock;

#[test]
fn readers_see_intact_bytes_across_merge() {
    // Build a Merge-policy cache, fill several segments in one TTL bucket
    // with known key→value pairs, then run merges while readers pin and
    // verify items. A pinned reader must always read the value that maps
    // to its key (either the pre-merge copy it pinned, or the relocated
    // copy the hashtable now points to) — never garbage from a byte that
    // was moved under its pointer.
    // ... build cache, insert N known items across M segments ...
    // ... spawn readers: loop { pick key; if let Some((item, _guard)) =
    //         segments.read().acquire_item_at(seg, off) { assert value bytes
    //         match the key's expected value; hold guard briefly } }
    // ... evictor: loop { segments.write().merge over the bucket } ...
    // ... join; assert no leak: free + spare + chain == total segments ...
}
```

Write the concrete test filling in the details from the real `acquire_item_at` return type (`Option<(RawItem, SegmentGuard)>`) and the insert API. Verify the value bytes via `RawItem`'s value accessor. The key assertion: **every successful `acquire_item_at` yields bytes consistent with the key** (no torn/relocated read). Add a no-leak assertion after joining.

Register in `segments/mod.rs`:
```rust
#[cfg(all(test, not(feature = "loom")))]
mod eviction_concurrency_tests;
```

- [ ] **Step 2: Run repeatedly**

Run: `cargo test -p segcache readers_see_intact_bytes_across_merge --release`
Then loop 20×:
```bash
for i in $(seq 1 20); do cargo test -p segcache readers_see_intact_bytes_across_merge --release 2>/dev/null | grep -q "test result: ok" || { echo "FAIL run $i"; break; }; done; echo done
```
Expected: 20/20 PASS.

- [ ] **Step 3: Bite-check — prove the test has teeth**

Temporarily reintroduce an in-place move: in `merge_evict`, before draining a candidate, add a line that relocates the candidate's first live item toward offset 0 in place (simulating the old `compact()` hazard) WITHOUT going through the hashtable relink — e.g. `unsafe { std::ptr::copy(cand.data_ptr().add(k), cand.data_ptr(), n) }` for a small region a reader could be looking at. Run the stress test in release. Expected: FAIL (a reader observes bytes inconsistent with its key, or a magic/torn-read assertion fires). RESTORE by re-editing (never `git checkout`). Re-run to confirm green. Report the failure message observed.

If a clean, representative in-place mutation is hard to inject without large scaffolding, instead bite-check by making `clear_segment` skip the `ref_count` recheck (free a pinned segment) and show the no-leak / use-after-free assertion fires — whichever most directly demonstrates the reader-safety property has teeth. Document which bite you used.

- [ ] **Step 4: Battery + commit**

Run: `cargo test -p segcache && cargo test -p segcache --features debug && cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --all --check`

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs crates/segcache/src/segments/mod.rs
git commit -m "Add reader-vs-merge safety stress test"
```
(with the standard footer)

---

### Task 6: Scope re-tagging, state comments, full verification

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs`, `crates/segcache/src/segments/segments.rs` (SCOPE re-tags)
- Modify: `crates/segcache/src/segments/state.rs` (Relinking/Locked comment)

- [ ] **Step 1: Re-tag the deferred SCOPE comments**

The two remaining `SCOPE(item-5)` comments cover the writer-vs-drain hazard, which this PR does NOT close. Re-tag them so they don't read as done:
- `ttl_bucket.rs:145` (`drain_chain`): change `SCOPE(item-5)` → `SCOPE(writer-vs-drain)` and note it is deferred past drain-safe merge (item 5b), still gated by `&mut`-serialized writers until item 7.
- `segments.rs:248` (`try_alloc_item`): same re-tag.

- [ ] **Step 2: Update the Relinking/Locked state docs**

In `state.rs` (the `Relinking` and `Locked` doc comments ~lines 21-26), keep them declared-unused but update the note: the copy-to-drain merge (this PR) and s3fifo made an in-place `Relinking`/`Locked` protocol unnecessary under serialized eviction; they remain reserved for a future concurrent-eviction design.

- [ ] **Step 3: Full verification battery**

```bash
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --features loom -- loom
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
```
Expected: all PASS (loom count unchanged from item 4 — no new loom models this PR), no warnings, no format diffs. Report exact counts.

- [ ] **Step 4: Bench guard**

```bash
cargo bench -p segcache -- set 2>&1 | tail -30
cargo bench -p segcache -- incr 2>&1 | tail -20
```
Record `set/1b/1b` and `incr/hot_counter`. The reserve/read hot paths are unchanged by this PR, so expect no movement vs the item-4 numbers (`set/1b/1b` ~40ns, `incr` ~38ns). If a bench that touches eviction exists, run it too. Record numbers for the PR description; investigate (re-run once) only if a hot-path bench moves > ±2ns.

- [ ] **Step 5: Commit**

```bash
git add -A crates/segcache/src
git commit -m "Re-tag deferred writer-vs-drain scope; document unused Relinking/Locked"
```
(with the standard footer)

---

### Task 7: Finish

- [ ] **Step 1: Review the diff against the spec**

Run `git diff main --stat` and re-read `docs/superpowers/specs/2026-07-17-drain-safe-merge-design.md`. Map each spec section to landed code: §1→Tasks 3-4, §2→Task 1, §3→Task 2, §4→Task 5, §5→Task 6. Confirm `Segment::compact` is gone and no eviction path mutates readable bytes in place.

- [ ] **Step 2: Final full-branch review**

Dispatch a whole-diff adversarial review (cross-commit consistency; the reader-safety argument for the copy-to-spare path; spare-queue accounting balance across reserve_spare/return_segment/recycle/condemn/guard-drop; metrics; graceful-fallback correctness; no behavior change for the simple/s3fifo policies).

- [ ] **Step 3: Use the finishing-a-development-branch skill**

Invoke `superpowers:finishing-a-development-branch` to push and open a PR against `pelikan-io/cache-rs` (upstream, cross-fork `--repo pelikan-io/cache-rs --head brayniac:drain-safe-merge`), matching how item 4 (#30) landed.
