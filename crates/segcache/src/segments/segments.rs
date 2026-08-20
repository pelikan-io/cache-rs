//! The `Segments` collection — owns all segment headers and mmap-backed data,
//! manages the free queue, and implements eviction execution (merge, S3-FIFO,
//! random, etc.).

use crate::eviction::*;
use crate::segments::segment::SEG_MAGIC;
use crate::segments::*;
use crate::sync::Ordering;
use crate::*;
use core::hash::{BuildHasher, Hasher};
use core::num::NonZeroU32;
use crossbeam_utils::Backoff;
use memmap2::MmapOptions;

/// `Segments` contain all items within the cache. This struct is a collection
/// of individual `Segment`s which are represented by a `SegmentHeader` and a
/// subslice of bytes from a contiguous anonymous mmap allocation.
pub(crate) struct Segments {
    /// Segment metadata headers (one per segment, cache-line aligned).
    ///
    /// STABILITY: allocated once in `from_builder` and never reassigned
    /// or resized. `SegmentGuard` (and `RawItem` for the data mmap
    /// below) hold raw pointers into this allocation, valid because a
    /// boxed slice's heap storage is stable even if `Segments` moves.
    headers: Box<[SegmentHeader]>,
    /// Anonymous mmap-backed heap for segment data.
    data: memmap2::MmapMut,
    /// Segment size in bytes.
    segment_size: i32,
    /// Total number of segments.
    cap: u32,
    /// Lock-free free segment queue. Boxed for a stable address: guards
    /// hold a raw pointer to it so the AwaitingRelease handoff can return
    /// segments without `&mut Segments`.
    free_queue: Box<crossbeam_deque::Injector<u32>>,
    /// Held-back spare segments for merge compaction. Never handed out by
    /// `reserve_free` (normal writes), so a destination is always available
    /// to merge even when the main free queue is empty.
    spare_queue: Box<crossbeam_deque::Injector<u32>>,
    /// Target number of segments to keep in the spare queue.
    spare_capacity: u32,
    /// Current spare-queue depth. `return_segment` replenishes it with a
    /// `compare_exchange` loop, so concurrent returners (a reader's
    /// guard-drop racing an evictor's recycle) never overfill it beyond
    /// `spare_capacity` — see `return_segment` and its loom model.
    spare_count: crate::sync::AtomicU32,
    /// Cached eviction policy. `Policy` is `Copy` and set once in
    /// `from_builder`; caching it here lets `evict_policy()` (on the reserve
    /// hot path) read it without taking the eviction lock.
    policy: Policy,
    /// Eviction mutable state (`rng`, `ranked_segs`/`index`, `ghost`,
    /// `last_update_time`), serialized behind a `std::sync::Mutex`. Eviction
    /// is not loom-modeled — the lock is what serializes it — so the std
    /// mutex is correct here (not a loom mutex). The lock is taken per-call
    /// (short-lived), NOT held across a whole eviction. Two evictors may thus
    /// redundantly select the same candidate; that is harmless, because
    /// per-segment *data-mutation* exclusivity comes from the `Sealed ->
    /// Draining` CAS (`claim_for_drain`, run before any mutation), NOT from a
    /// single-evictor lock. See the soundness contract on `segment()`.
    ///
    /// Lock order: this policy lock is INNER to `TtlBucket::chain_lock` — code
    /// may take `evict` while holding a bucket's `chain_lock`, never the
    /// reverse (no site acquires a `chain_lock` while holding `evict`).
    // LOCK: eviction-policy
    evict: std::sync::Mutex<Eviction>,
    /// Max segments in the admission pool (S3-FIFO only, 0 for other policies).
    admission_cap: u32,
    /// Current number of segments in the admission pool.
    admission_count: crate::sync::AtomicU32,
}

/// Result of draining a segment: `Freed` means it was returned to the
/// free queue; `Deferred` means it was condemned to AwaitingRelease and
/// the last reader's guard drop will free it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearOutcome {
    Freed,
    Deferred,
}

/// Outcome of a pinned allocation attempt on a specific tail segment.
#[derive(Debug)]
pub(crate) enum AllocOutcome {
    /// Space granted; the item is pinned (`WriterPin` inside).
    Reserved(ReservedItem),
    /// Segment is `Live` but full — the caller should expand the chain.
    Full,
    /// Segment is no longer writable (raced a seal/claim) — the caller should
    /// re-read the tail rather than expand.
    NotWritable,
}

impl Segments {
    /// Allocate and initialize segments by consuming the builder. The backing
    /// heap is an anonymous mmap region instead of a boxed slice so that large
    /// caches do not fragment the process heap.
    pub(super) fn from_builder(builder: SegmentsBuilder) -> Result<Self, SegmentsError> {
        let segment_size = builder.segment_size;
        let segments = builder.heap_size / (segment_size as usize);

        debug!(
            "heap size: {} seg size: {} segments: {}",
            builder.heap_size, segment_size, segments
        );

        assert!(
            segments < (1 << 24),
            "heap size requires too many segments, reduce heap size or increase segment size"
        );

        let evict_policy = builder.evict_policy;

        debug!("eviction policy: {evict_policy:?}");

        // Build headers array.
        let mut headers = Vec::with_capacity(0);
        headers.reserve_exact(segments);
        for idx in 0..segments {
            // SAFETY: idx + 1 is always >= 1 and constrained to < 2^24.
            let header = SegmentHeader::new(unsafe { NonZeroU32::new_unchecked(idx as u32 + 1) });
            headers.push(header);
        }
        let headers = headers.into_boxed_slice();

        // Allocate the data heap via anonymous mmap.
        let heap_size = segments * segment_size as usize;
        let mut data = MmapOptions::new().populate().len(heap_size).map_anon()?;

        // A Merge-policy cache holds back one segment as a copy
        // destination for compaction, always available even when the
        // normal free queue is drained to empty. Other policies never
        // compact in place, so they need no spare.
        let spare_capacity: u32 = if matches!(evict_policy, Policy::Merge { .. }) {
            1
        } else {
            0
        };

        // Initialize each segment and fill the free/spare queues. Segments
        // rest in the Free state with no chain links; both queues are
        // lock-free Injectors rather than intrusive lists.
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

        #[cfg(feature = "metrics")]
        {
            SEGMENT_CURRENT.set(segments as _);
            SEGMENT_FREE.set(segments as _);
        }

        let admission_cap = if let Policy::S3Fifo { admission_ratio } = evict_policy {
            (segments as f64 * admission_ratio).round() as u32
        } else {
            0
        };

        Ok(Self {
            headers,
            segment_size,
            cap: segments as u32,
            free_queue,
            spare_queue,
            spare_capacity,
            spare_count: crate::sync::AtomicU32::new(spare_capacity),
            data,
            policy: evict_policy,
            evict: std::sync::Mutex::new(Eviction::new(segments, evict_policy)),
            admission_cap,
            admission_count: crate::sync::AtomicU32::new(0),
        })
    }

    // ── Pool helpers ─────────────────────────────────────────────────

    /// Check if the given pool has room for another segment.
    pub(crate) fn pool_has_room(&self, pool: SegmentPool) -> bool {
        match pool {
            SegmentPool::Admission => {
                self.admission_count.load(Ordering::Relaxed) < self.admission_cap
            }
            SegmentPool::Main => true,
        }
    }

    /// Track a segment transitioning to the given pool.
    pub(crate) fn incr_pool(&self, pool: SegmentPool) {
        if pool == SegmentPool::Admission {
            self.admission_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// Return the configured eviction policy.
    #[inline]
    pub fn evict_policy(&self) -> Policy {
        self.policy
    }

    /// Return the size of each segment in bytes.
    #[inline]
    pub fn segment_size(&self) -> i32 {
        self.segment_size
    }

    /// Create a `SegmentsVerifier` for key verification in the hashtable.
    pub(crate) fn verifier(&self) -> SegmentsVerifier<'_> {
        SegmentsVerifier::new(
            &self.data[..],
            self.segment_size as usize,
            self.cap as usize,
        )
    }

    /// Returns the number of available segments (free queue + spare
    /// queue).
    #[cfg(test)]
    pub fn free(&self) -> usize {
        self.free_queue.len() + self.spare_queue.len()
    }

    /// Segments available to normal writes (the general free queue only,
    /// excluding the held-back spare). Used by `reserve_and_define` as an
    /// eviction-progress signal: if an `evict()` pass does not raise this, it
    /// freed nothing a reserve can use (e.g. a merge that only refilled the
    /// spare), so the retry loop must not spin on it. `Injector::len` is an
    /// estimate, which is fine here — a stale read only costs a bounded extra
    /// retry or an early give-up, never an unbounded loop.
    pub(crate) fn free_queue_len(&self) -> usize {
        self.free_queue.len()
    }

    /// Returns the number of segments available to normal writes (free
    /// queue only, excluding the held-back spare).
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn free_only(&self) -> usize {
        self.free_queue.len()
    }

    /// Target number of segments held back in the spare queue.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn spare_capacity(&self) -> u32 {
        self.spare_capacity
    }

    /// Current spare-queue depth.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn spare_count(&self) -> u32 {
        self.spare_count.load(Ordering::Relaxed)
    }

    /// Shared access to a segment's header by id.
    #[inline]
    pub(crate) fn header(&self, id: NonZeroU32) -> &SegmentHeader {
        &self.headers[id.get() as usize - 1]
    }

    /// Iterate every segment header, for asserting global invariants (e.g.
    /// no leaked writer pins) after a sequence of operations.
    #[cfg(test)]
    pub(crate) fn iter_headers_for_test(&self) -> impl Iterator<Item = &SegmentHeader> {
        self.headers.iter()
    }

    /// Returns a segment's creation time and TTL, read directly from the
    /// header — no `Segment` view construction or magic-byte check, since
    /// this sits on the numeric-op hot path.
    #[inline]
    pub(crate) fn expiry_info(&self, seg_id: NonZeroU32) -> (Instant, Duration) {
        let header = self.header(seg_id);
        (header.create_at(), header.ttl())
    }

    /// Returns the generation counter for a segment. Bumped each time the
    /// segment is returned to the free queue, so CAS tokens built from it
    /// are invalidated when the segment is recycled.
    #[inline]
    pub(crate) fn generation(&self, seg_id: NonZeroU32) -> u16 {
        self.header(seg_id).generation()
    }

    // ── Item access ──────────────────────────────────────────────────

    /// Retrieve a `RawItem` from a specific segment id at the given offset.
    /// This can take `&self` because we only need a shared reference to the
    /// header and we construct the `RawItem` directly from a data pointer.
    pub(crate) fn get_item_at(&self, seg_id: Option<NonZeroU32>, offset: usize) -> Option<RawItem> {
        let seg_id = seg_id.map(|v| v.get())?;
        trace!("getting item from: seg: {seg_id} offset: {offset}");
        assert!(seg_id <= self.cap);

        let byte_offset = self.segment_size() as usize * (seg_id as usize - 1) + offset;
        Some(RawItem::from_ptr(unsafe {
            (self.data.as_ptr() as *mut u8).add(byte_offset)
        }))
    }

    /// Like [`Self::get_item_at`], but pins the segment with a reader
    /// guard first. While the guard is alive the segment cannot be
    /// recycled, merged, or compacted. Returns `None` if the segment is
    /// not in a readable state.
    pub(crate) fn acquire_item_at(
        &self,
        seg_id: NonZeroU32,
        offset: usize,
    ) -> Option<(RawItem, SegmentGuard)> {
        assert!(seg_id.get() <= self.cap);
        let header = &self.headers[seg_id.get() as usize - 1];

        match header.try_acquire_reader() {
            super::AcquireOutcome::Acquired => {}
            super::AcquireOutcome::NotReadable => return None,
            super::AcquireOutcome::ReleaseCondemned => {
                // Backing the pin out left a condemned segment with no
                // reader remaining, and this caller won its release.
                //
                // Settle the accounting FIRST, exactly as the other two
                // claimants of that CAS do (`recycle`/the last guard drop,
                // and `condemn`'s race-fix recheck): the won CAS already
                // flipped the word to `Free` with `ref_count` at zero and
                // nothing can find the segment until the push below, so we
                // are its sole owner here. Skipping it puts a segment on
                // the free queue still carrying its whole dead charge and
                // any unpinned-unlink live residue, leaving `ITEM_DEAD` /
                // `ITEM_DEAD_BYTES` above the true dead occupancy — and
                // `ITEM_CURRENT`/`ITEM_CURRENT_BYTES` above the true live
                // count — until some later `try_reserve` happens to pick
                // that segment up, which for an idle cache is never (issue
                // #58 part 2). `reset_write_stats` is idempotent, so the
                // reserve-time reset stays harmless.
                header.reset_write_stats();

                self.return_segment(seg_id.get());

                #[cfg(feature = "metrics")]
                {
                    SEGMENT_RETURN.increment();
                    SEGMENT_FREE.increment();
                }
                return None;
            }
        }
        // SAFETY: the acquire above succeeded, and both `headers` (a
        // boxed slice owned by `self`) and the boxed Injector outlive
        // any guard reachable through the public API.
        let guard = unsafe { SegmentGuard::new(header, &*self.free_queue) };

        let byte_offset = self.segment_size() as usize * (seg_id.get() as usize - 1) + offset;
        let raw = RawItem::from_ptr(unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) });
        Some((raw, guard))
    }

    /// Atomically reserve space for an item in the given segment, pinning
    /// it as a writer for the reserve→publish window (item 7d's Dekker
    /// pair). Returns an `AllocOutcome`: `Reserved` on a granted region,
    /// `Full` if the segment is `Live` but out of space (caller should
    /// expand the chain), or `NotWritable` if the segment stopped being
    /// writable (raced a seal/claim — caller should re-read the tail).
    ///
    /// Takes `&self`: the reservation is a header CAS and the item
    /// pointer is derived from the data base pointer, the same pattern
    /// as `get_item_at`.
    ///
    /// The `integrity` magic-byte check is intentionally skipped here
    /// (hot path); the debug-feature `check_integrity` scan covers it,
    /// the same idiom as `expiry_info`.
    pub(crate) fn try_alloc_item(&self, seg_id: NonZeroU32, size: i32) -> AllocOutcome {
        debug_assert!(seg_id.get() <= self.cap);
        let header = self.header(seg_id);

        // Writer half of the Dekker pair (item 7d): pin before touching
        // write_offset, and bail if the segment stopped being writable.
        if !header.try_pin_writer() {
            return AllocOutcome::NotWritable;
        }
        // SAFETY: try_pin_writer returned true; the headers allocation outlives
        // this pin (the ReservedItem is consumed within the caller's insert/cas).
        let pin = unsafe { WriterPin::new(header as *const _) };

        let offset = match header.try_reserve_space(size, self.segment_size) {
            Some(offset) => offset,
            None => return AllocOutcome::Full, // pin dropped here → released
        };

        header.incr_live_items();
        header.incr_live_bytes(size);

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.increment();
            ITEM_CURRENT_BYTES.add(size as _);
            ITEM_ALLOCATE.increment();
        }

        let byte_offset =
            self.segment_size as usize * (seg_id.get() as usize - 1) + offset as usize;
        // SAFETY: `header()` above bounds-checks seg_id via slice
        // indexing, and the CAS grant guarantees
        // `offset + size <= segment_size`, so the granted region lies
        // inside this segment's slice of the data mmap.
        let ptr = unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) };
        AllocOutcome::Reserved(ReservedItem::new(
            RawItem::from_ptr(ptr),
            seg_id,
            offset as usize,
            pin,
        ))
    }

    /// Pin a segment for a replace/delete about to unlink+decrement one of
    /// its items (item 7f). Returns the guard if the segment is removable
    /// (Sealed/Live), `None` if a drain claimed it in the interim (the
    /// caller should re-look-up the key and retry). SAFETY: the headers
    /// allocation outlives the pin (`Segments::headers` is never resized or
    /// reassigned after construction).
    pub(crate) fn try_pin_remover(&self, seg_id: NonZeroU32) -> Option<RemoverPin> {
        let header = self.header(seg_id);
        if header.try_pin_remover() {
            // SAFETY: try_pin_remover returned true; headers outlive the pin.
            Some(unsafe { RemoverPin::new(header as *const _) })
        } else {
            None
        }
    }

    // ── Segment views ────────────────────────────────────────────────

    /// Returns a `Segment` view for the segment with the specified id.
    ///
    /// # Why `&self` is sound (exclusivity by segment state-ownership)
    ///
    /// This hands out `&mut [u8]` access to a segment's data region from
    /// `&self`. It is sound because mutable access to a given segment's data is
    /// exclusive by the segment's *state ownership*, not by any global lock:
    /// - the reserver of the `Live` tail is the only writer of that tail (writes
    ///   are placed at CAS-allocated, disjoint offsets — `try_alloc_item`);
    /// - a candidate is claimed for mutation (drain/merge-copy) only by the
    ///   single thread that wins its `Sealed -> Draining` CAS BEFORE mutating;
    ///   losers see a non-Sealed state and skip it. This is the uniform claim for
    ///   ALL candidate mutators (merge, s3fifo, drop, expire/clear, remove_at).
    /// - a `Reserved` spare is owned by the one evictor that reserved it;
    /// - readers only read (via `acquire_item_at` pins); the copy-then-publish
    ///   ordering (7a) + the pin/condemn protocol keep bytes valid for them.
    ///
    /// So no two threads ever hold `&mut` to the same segment's region at once.
    ///
    /// Drain-first (item 7c Task 6, DONE): `merge_evict`/`merge_compact`/
    /// `s3fifo_evict_admission`/`s3fifo_evict_main` now win the candidate's
    /// `Sealed -> Draining` CAS (`claim_for_drain`) BEFORE any `prune`/`copy_into`/
    /// `remove_item_at`, then `finalize_drained` (no second CAS). So the
    /// "claim before mutate" rule above holds literally for every mutator, and
    /// the eviction receivers are `&self`: two evictors that deterministically
    /// select the same `Sealed` candidate cannot both derive `&mut` to it — the
    /// loser's `claim_for_drain` returns false and it skips.
    ///
    /// The reserver-vs-drain race on a `Live` tail is closed by item 7d: a
    /// reserver pins the segment as a writer (`try_pin_writer`, incrementing
    /// `active_writers`) across its whole reserve→define→publish window, and
    /// every parse site (`drain_chain`, `claim_for_drain`) waits for
    /// `active_writers == 0` after winning its state CAS — the claimer half of
    /// the same SeqCst Dekker pair the reader pin uses. So a drain never parses
    /// a reserved-but-undefined region (H1), and a reserver never writes or
    /// publishes into a recycled segment (H2). The seal itself re-checks the
    /// tail's generation under `chain_lock` before firing (H3), so a recycled,
    /// reused tail is never sealed.
    pub(crate) fn segment(&self, id: NonZeroU32) -> Result<Segment<'_>, SegmentsError> {
        let idx = id.get() as usize - 1;
        if idx < self.headers.len() {
            let header = &self.headers[idx];

            let seg_start = self.segment_size as usize * idx;

            // SAFETY: idx is in bounds; per the state-ownership contract above,
            // the region [seg_start, seg_start + seg_size) is exclusively owned
            // by this caller for the view's lifetime. The mmap base pointer is
            // stable for the life of `self` (the allocation is never resized).
            let seg_data = unsafe {
                std::slice::from_raw_parts_mut(
                    (self.data.as_ptr() as *mut u8).add(seg_start),
                    self.segment_size as usize,
                )
            };

            let segment = Segment::from_raw_parts(header, seg_data);
            segment.check_magic();
            Ok(segment)
        } else {
            Err(SegmentsError::BadSegmentId)
        }
    }

    /// Returns `Segment` views for two DISTINCT segments. `&self` for the same
    /// reason as `segment` — the two data regions are disjoint (a != b), and
    /// each is exclusively owned per the state-ownership contract on `segment`.
    pub(crate) fn segment_pair(
        &self,
        a: NonZeroU32,
        b: NonZeroU32,
    ) -> Result<(Segment<'_>, Segment<'_>), SegmentsError> {
        if a == b {
            return Err(SegmentsError::BadSegmentId);
        }

        let a_idx = a.get() as usize - 1;
        let b_idx = b.get() as usize - 1;
        if a_idx >= self.headers.len() || b_idx >= self.headers.len() {
            return Err(SegmentsError::BadSegmentId);
        }

        // Headers are shared refs — aliasing is fine.
        let header_a = &self.headers[a_idx];
        let header_b = &self.headers[b_idx];

        let seg_size = self.segment_size as usize;
        let base = self.data.as_ptr() as *mut u8;

        // SAFETY: a_idx != b_idx, so [a_idx*seg_size, +seg_size) and
        // [b_idx*seg_size, +seg_size) are disjoint; the two &mut slices never
        // alias. Each region is exclusively owned per the contract on `segment`.
        let (data_a, data_b) = unsafe {
            (
                std::slice::from_raw_parts_mut(base.add(a_idx * seg_size), seg_size),
                std::slice::from_raw_parts_mut(base.add(b_idx * seg_size), seg_size),
            )
        };

        let segment_a = Segment::from_raw_parts(header_a, data_a);
        let segment_b = Segment::from_raw_parts(header_b, data_b);

        segment_a.check_magic();
        segment_b.check_magic();
        Ok((segment_a, segment_b))
    }

    // ── Chain helpers ────────────────────────────────────────────────

    /// Unlink a segment from its chain by patching the prev/next pointers of
    /// its neighbours.
    ///
    /// *NOTE*: this must not be used on segments in the free queue.
    fn unlink(&self, id: NonZeroU32) {
        let id_idx = id.get() as usize - 1;

        if let Some(next) = self.headers[id_idx].next_seg() {
            let prev = self.headers[id_idx].prev_seg();
            self.headers[next.get() as usize - 1].set_prev_seg(prev);
        }

        if let Some(prev) = self.headers[id_idx].prev_seg() {
            let next = self.headers[id_idx].next_seg();
            self.headers[prev.get() as usize - 1].set_next_seg(next);
        }
    }

    /// Link a Reserved copy-destination segment (merge spare / s3fifo target)
    /// at the front of a chain and publish it as `Relinking`: Reserved ->
    /// Linking carries the next pointer, the old head's prev is patched, then
    /// Linking -> Relinking publishes (never the write tail — the tail is Live).
    ///
    /// `Relinking` is readable (survivors relinked into the destination via
    /// `cas_location` stay reachable to readers) but NOT evictable (only
    /// `Sealed` is). So while the owner fills the destination across its copy
    /// loop, no concurrent evictor can select it (`can_evict` is false, being
    /// header-only via `is_evictable`) or win its `Sealed->Draining` claim
    /// (`claim_for_drain` CASes from `Sealed`, which a `Relinking` segment is
    /// not). The owner calls `publish_dest_sealed` (Relinking -> Sealed) once
    /// the fill completes, making the destination a legal future candidate.
    fn link_dest_at_head(&self, this: NonZeroU32, head: Option<NonZeroU32>) {
        let this_idx = this.get() as usize - 1;
        let linking = self.headers[this_idx].cas_metadata(
            State::Reserved,
            State::Linking,
            Some(head),
            Some(None),
            Ordering::AcqRel,
        );
        debug_assert!(linking, "head insert requires a Reserved segment");

        if let Some(head_id) = head {
            let head_idx = head_id.get() as usize - 1;
            debug_assert!(self.headers[head_idx].prev_seg().is_none());
            self.headers[head_idx].update_links(None, Some(Some(this)));
        }

        let relinking = self.headers[this_idx].cas_metadata(
            State::Linking,
            State::Relinking,
            None,
            None,
            Ordering::AcqRel,
        );
        debug_assert!(relinking, "linking destination must publish as Relinking");
    }

    /// Publish a filled copy-destination: `Relinking -> Sealed`, making it a
    /// legal future eviction candidate. Must be called on EVERY merge/s3fifo
    /// exit path after the fill loop, so a destination is never left stuck in
    /// `Relinking` (unclaimable, hence unrecyclable). Sealing an empty
    /// destination (zero candidates merged) is fine — it matches the
    /// pre-existing empty-spare-as-head behavior.
    fn publish_dest_sealed(&self, id: NonZeroU32) {
        let sealed = self.headers[id.get() as usize - 1].cas_metadata(
            State::Relinking,
            State::Sealed,
            None,
            None,
            Ordering::AcqRel,
        );
        debug_assert!(sealed, "copy destination must publish Relinking -> Sealed");
    }

    // ── Free queue ───────────────────────────────────────────────────

    /// Return a drained segment to the free queue. The segment must be in
    /// the Draining state with no readers pinning it; its write statistics
    /// are reset (and its generation bumped) at reserve time.
    pub(crate) fn recycle(&self, id: NonZeroU32) {
        let id_idx = id.get() as usize - 1;
        debug_assert_eq!(
            self.headers[id_idx].ref_count(),
            0,
            "freed a segment pinned by readers"
        );

        // Unlink from its chain first: this reads the segment's own
        // prev/next to patch the neighbors, so it must happen before the
        // transition below clears the links.
        self.unlink(id);

        // Pool membership ends when the segment leaves service.
        if self.headers[id_idx].pool() == SegmentPool::Admission {
            let prev = self.admission_count.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(prev > 0, "admission_count underflowed in recycle");
        }
        self.headers[id_idx].set_pool(SegmentPool::Main);

        // Reset the write statistics while the segment is still exclusively
        // ours (Draining, reader count observed zero): removals that
        // unlinked hashtable entries WITHOUT a remover pin (a delete racing
        // this drain, the fresh-key insert de-dup race, a reservation
        // rollback) could not decrement the counters, and the drain sweep
        // skipped those already-unlinked items, so `live_*`/`write_offset`
        // may be transiently over-counted (see the item 7f note on
        // `Segment::clear`). Resetting here keeps a Free segment reporting
        // zero items (`items()`) and off the dead gauges; `try_reserve`
        // repeats the reset (idempotently) when the segment is handed to its
        // next tenant. The condemned path does not pass through here, so it
        // resets at its own free sites instead — see `reset_write_stats`.
        self.headers[id_idx].reset_write_stats();

        let freed = self.headers[id_idx].cas_metadata(
            State::Draining,
            State::Free,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        );
        debug_assert!(freed, "recycled a segment that was not Draining");

        self.return_segment(id.get());

        #[cfg(feature = "metrics")]
        {
            SEGMENT_RETURN.increment();
            SEGMENT_FREE.increment();
        }
    }

    /// Reserve a segment from the free queue. Returns the id of a
    /// segment in the Reserved state (statistics reset, generation
    /// bumped), which must then be linked into a segment chain.
    pub(crate) fn reserve_free(&self) -> Option<NonZeroU32> {
        loop {
            match self.free_queue.steal() {
                crossbeam_deque::Steal::Retry => continue,
                crossbeam_deque::Steal::Empty => return None,
                crossbeam_deque::Steal::Success(raw) => {
                    debug_assert!(raw >= 1 && raw <= self.cap);
                    let id = NonZeroU32::new(raw)?;
                    if self.headers[raw as usize - 1].try_reserve() {
                        #[cfg(feature = "metrics")]
                        {
                            SEGMENT_REQUEST.increment();
                            SEGMENT_REQUEST_SUCCESS.increment();
                            SEGMENT_FREE.decrement();
                        }
                        return Some(id);
                    }
                    // Not actually Free (a transient state raced through
                    // the queue) — put it back and let the caller retry
                    // or run eviction.
                    self.free_queue.push(raw);
                    return None;
                }
            }
        }
    }

    /// Reserve a segment for merge compaction. Prefers the held-back
    /// spare queue; falls back to the normal free queue when the spare is
    /// empty. Returns a `Reserved` segment, like `reserve_free`. Called by
    /// `merge_evict` and `merge_compact` to obtain their copy-to-spare
    /// destination.
    pub(crate) fn reserve_spare(&self) -> Option<NonZeroU32> {
        loop {
            match self.spare_queue.steal() {
                crossbeam_deque::Steal::Retry => continue,
                crossbeam_deque::Steal::Empty => return self.reserve_free(),
                crossbeam_deque::Steal::Success(raw) => {
                    debug_assert!(raw >= 1 && raw <= self.cap);
                    let id = NonZeroU32::new(raw)?;
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
                    // Not actually Free (a transient state raced through
                    // the queue) — put it back and let the caller retry
                    // or run eviction.
                    self.spare_queue.push(raw);
                    return None;
                }
            }
        }
    }

    /// Return a segment id to the pool, replenishing the held-back spare
    /// queue before the normal free queue. Callers push the id after
    /// their own state transition to Free; this only decides which queue
    /// it lands in.
    ///
    /// Concurrency-safe: the CAS ensures exactly one returner bumps
    /// `spare_count` into each slot, so the spare queue never overfills
    /// beyond `spare_capacity` even when a reader's guard-drop races an
    /// evictor's recycle. See `loom_tests::loom_return_segment_no_overfill`.
    fn return_segment(&self, id: u32) {
        let mut count = self.spare_count.load(Ordering::Relaxed);
        loop {
            if count >= self.spare_capacity {
                self.free_queue.push(id);
                return;
            }
            match self.spare_count.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.spare_queue.push(id);
                    return;
                }
                Err(observed) => count = observed,
            }
        }
    }

    /// Return a Reserved (or Linking) segment that was never published
    /// into a chain — the loser path of the chain-extension election
    /// and allocation error paths.
    pub(crate) fn release_unused(&self, id: NonZeroU32) {
        let released = self.headers[id.get() as usize - 1].try_release();
        assert!(
            released,
            "release_unused on a segment not in Reserved/Linking"
        );
        self.return_segment(id.get());

        #[cfg(feature = "metrics")]
        {
            SEGMENT_RETURN.increment();
            SEGMENT_FREE.increment();
        }
    }

    // ── Eviction ─────────────────────────────────────────────────────

    /// Drain a Sealed segment and return it to the free queue, or
    /// condemn it to the last reader if it is pinned.
    ///
    /// Crucible's order: transition to Draining FIRST (the exclusivity
    /// CAS — fails cleanly on the Live tail or a concurrently drained
    /// segment), drain the hashtable, THEN check the reader count. A
    /// drained segment leaves its chain either way; the caller must do
    /// bucket head fixup using links captured before this call.
    ///
    /// `Deferred` means the segment was drained and condemned but its
    /// memory is not yet reclaimable — the last reader's guard drop will
    /// free it.
    fn clear_segment(
        &self,
        id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
        expire: bool,
    ) -> Result<ClearOutcome, ()> {
        if self.claim_for_drain(id) {
            Ok(self.finalize_drained(id, hashtable, expire))
        } else {
            Err(())
        }
    }

    /// Claim a Sealed segment for draining by winning its `Sealed -> Draining`
    /// CAS. Returns `true` iff this thread won (the segment was Sealed and is
    /// now Draining, exclusively owned by this thread). Losers get `false`
    /// (the segment was the Live tail, already Draining, or concurrently
    /// claimed by another mutator).
    ///
    /// This is the single, uniform mutation claim for ALL candidate mutators
    /// (merge, s3fifo, drop, expire/clear, remove_at's empty-free): a thread
    /// must win this CAS BEFORE mutating a candidate's data region, so the
    /// `segment()` accessor's exclusivity contract holds even under concurrent
    /// evictors deterministically selecting the same candidate.
    fn claim_for_drain(&self, id: NonZeroU32) -> bool {
        let id_idx = id.get() as usize - 1;

        // SeqCst: claimer half of the Dekker pair with try_acquire_reader
        // (transition the state, then observe the reader count).
        let won = self.headers[id_idx].cas_metadata(
            State::Sealed,
            State::Draining,
            None,
            None,
            Ordering::SeqCst,
        );

        if won {
            // Writer half already ran its SeqCst pin+recheck: any reserver that
            // observed Live before our CAS is counted here; any that increments
            // after sees Draining and bails. Wait for the counted ones to finish
            // define+publish before we parse the item stream (item 7d, H1/H2).
            // Bounded, but no longer straight-line: a pinned writer's
            // publish can block on the hashtable's insert-stripe mutex
            // (fresh-key entry creation). Still no cycle — that stripe is
            // a LEAF lock, taken only around bucket-word CASes and
            // verifier reads, never while holding or waiting on anything
            // here. The snooze yields after a short spin so a descheduled
            // pin holder gets CPU on an oversubscribed host.
            let backoff = Backoff::new();
            while self.headers[id_idx].active_writers() != 0 {
                backoff.snooze();
            }
            // Item 7f: also wait for in-flight replace/delete removes of this
            // segment's items to finish decrementing before we parse/reclaim it
            // (claimer half of the remover Dekker pair). A remover that pins
            // after our claim CAS sees Draining (try_pin_remover recheck) and
            // bails, so this converges.
            while self.headers[id_idx].active_removers() != 0 {
                backoff.snooze();
            }
        }
        won
    }

    /// Test-only shim exposing the private `claim_for_drain` claim CAS + wait,
    /// so `eviction_concurrency_tests` can exercise it directly without
    /// widening the production method's visibility.
    #[cfg(test)]
    #[allow(dead_code)] // caller `eviction_concurrency_tests` is cfg'd out under loom
    pub(crate) fn claim_for_drain_for_test(&self, id: NonZeroU32) -> bool {
        self.claim_for_drain(id)
    }

    /// Test-only shim exposing the private `s3fifo_promote_from` (the second
    /// relocation site, alongside `Segment::copy_into`), so
    /// `dead_accounting_tests` can drive exactly one promotion and inspect the
    /// source's counters instead of inferring them from an eviction storm.
    #[cfg(test)]
    #[allow(dead_code)] // caller is cfg'd out under loom
    pub(crate) fn s3fifo_promote_from_for_test(
        &self,
        src_id: NonZeroU32,
        dst_id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
    ) {
        self.s3fifo_promote_from(src_id, dst_id, hashtable);
    }

    /// Test-only shim exposing the private `finalize_drained` (sweep the
    /// segment's remaining hashtable entries, then recycle or condemn it),
    /// the completion half of `claim_for_drain_for_test`. Lets a test park a
    /// segment mid-drain and later let the drain PROGRESS, which is what the
    /// writer-vs-drain rollback/restart loop waits on — a permanently parked
    /// segment would legitimately spin that loop forever and is not a valid
    /// liveness test.
    #[cfg(test)]
    #[allow(dead_code)] // callers are cfg'd out under loom
    pub(crate) fn finalize_drained_for_test(
        &self,
        id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
    ) -> ClearOutcome {
        self.finalize_drained(id, hashtable, false)
    }

    /// Finalize a segment this thread has already claimed (it is `Draining`,
    /// owned by this thread via `claim_for_drain`): capture its chain links,
    /// drain its remaining hashtable entries, then recycle it (ref_count == 0)
    /// or condemn it to the last reader's guard drop (pinned).
    ///
    /// Must only be called on a segment already in `Draining` — it does NOT
    /// re-run the `Sealed -> Draining` CAS. `expire` is threaded into
    /// `segment.clear` (expiry-metric bookkeeping).
    fn finalize_drained(
        &self,
        id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
        expire: bool,
    ) -> ClearOutcome {
        let id_idx = id.get() as usize - 1;

        // Capture the links before condemn clears them.
        let meta = self.headers[id_idx].metadata(Ordering::Acquire);
        let (next, prev) = (meta.next, meta.prev);

        {
            let mut segment = self.segment(id).unwrap();
            segment.clear(hashtable, expire);
        }

        if self.headers[id_idx].ref_count_seqcst() == 0 {
            self.recycle(id);
            ClearOutcome::Freed
        } else {
            self.condemn(id, next, prev)
        }
    }

    /// Condemn a drained, pinned segment: transition it to
    /// AwaitingRelease (chain-free) and hand reclamation to the last
    /// reader's guard drop. AwaitingRelease is not readable, so the pins
    /// handed off here are exactly those taken before this transition. The
    /// hashtable must already be fully drained for a separate reason: a
    /// reader whose pin now fails re-looks-up, and has to find nothing
    /// rather than resolve back into this segment.
    ///
    /// Returns `Freed` if the race-fix recheck discovered the last
    /// reader already dropped (this caller then reclaimed the segment),
    /// `Deferred` otherwise.
    pub(crate) fn condemn(
        &self,
        id: NonZeroU32,
        next: Option<NonZeroU32>,
        prev: Option<NonZeroU32>,
    ) -> ClearOutcome {
        let id_idx = id.get() as usize - 1;

        // Pool membership ends at condemn time (the guard drop has no
        // access to the pool bookkeeping).
        if self.headers[id_idx].pool() == SegmentPool::Admission {
            let prev_count = self.admission_count.fetch_sub(1, Ordering::Relaxed);
            debug_assert!(prev_count > 0, "admission_count underflowed in condemn");
        }
        self.headers[id_idx].set_pool(SegmentPool::Main);

        let condemned = self.headers[id_idx].cas_condemn();
        debug_assert!(condemned, "condemned a segment that was not Draining");

        // Splice the neighbors using the captured links (this segment's
        // own links were just cleared).
        if let Some(p) = prev {
            self.headers[p.get() as usize - 1].update_links(Some(next), None);
        }
        if let Some(n) = next {
            self.headers[n.get() as usize - 1].update_links(None, Some(prev));
        }

        // Race fix (crucible fifo_layer): if the last reader dropped
        // between the earlier ref_count check and the condemn CAS above,
        // the segment is AwaitingRelease with no readers and nobody will
        // free it. Recheck (SeqCst) and reclaim it ourselves; the CAS in
        // try_release_condemned keeps this exactly-one-free against a
        // racing guard drop.
        if self.headers[id_idx].ref_count_seqcst() == 0
            && self.headers[id_idx].try_release_condemned()
        {
            // Settle the accounting before the segment becomes reachable
            // again, exactly as `recycle` and the last guard drop do: the
            // won CAS makes us its sole owner, and a segment must not sit on
            // the free queue carrying live residue or dead occupancy (issue
            // #58 part 2). Idempotent, so the reserve-time reset still runs
            // harmlessly.
            self.headers[id_idx].reset_write_stats();

            self.return_segment(id.get());

            #[cfg(feature = "metrics")]
            {
                SEGMENT_RETURN.increment();
                SEGMENT_FREE.increment();
            }

            ClearOutcome::Freed
        } else {
            ClearOutcome::Deferred
        }
    }

    /// Perform eviction based on the configured eviction policy. A success
    /// indicates that a segment was put onto the free queue and `reserve_free()`
    /// should return some segment id.
    pub fn evict(
        &self,
        ttl_buckets: &TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Cheap path first: drop whole expired segments (no spare, no
        // copy). If any segment frees, a subsequent reserve_free will now
        // succeed, and the spare-consuming merge is skipped entirely.
        if ttl_buckets.expire(hashtable, self) > 0 {
            return Ok(());
        }

        #[cfg(feature = "metrics")]
        let now = Instant::now();

        match self.policy {
            Policy::Merge { .. } => {
                #[cfg(feature = "metrics")]
                SEGMENT_EVICT.increment();

                let mut seg_idx = self.evict.lock().unwrap().random();

                seg_idx %= self.cap;
                let ttl = self.headers[seg_idx as usize].ttl();
                let offset = ttl_buckets.get_bucket_index(ttl);
                let buckets = ttl_buckets.buckets.len();

                // Since merging starts in the middle of a segment chain, we
                // may need to loop back around to the first TTL bucket.
                for i in 0..=buckets {
                    let bucket_id = (offset + i) % buckets;
                    let ttl_bucket = &ttl_buckets.buckets[bucket_id];
                    if let Some(first_seg) = ttl_bucket.head() {
                        let start = ttl_bucket.next_to_merge().unwrap_or(first_seg);
                        match self.merge_evict(start, ttl_bucket, hashtable) {
                            Ok(next_to_merge) => {
                                debug!("merged ttl_bucket: {bucket_id} seg: {start}");
                                ttl_bucket.set_next_to_merge(next_to_merge);

                                #[cfg(feature = "metrics")]
                                EVICT_TIME.add(now.elapsed().as_nanos() as _);

                                return Ok(());
                            }
                            Err(_) => {
                                #[cfg(feature = "metrics")]
                                SEGMENT_EVICT_EX.increment();

                                ttl_bucket.set_next_to_merge(None);
                                continue;
                            }
                        }
                    }
                }

                #[cfg(feature = "metrics")]
                {
                    SEGMENT_EVICT_EX.increment();
                    EVICT_TIME.add(now.elapsed().as_nanos() as _);
                }

                Err(SegmentsError::NoEvictableSegments)
            }
            Policy::S3Fifo { .. } => {
                #[cfg(feature = "metrics")]
                SEGMENT_EVICT.increment();

                let result = self.s3fifo_evict(ttl_buckets, hashtable);

                #[cfg(feature = "metrics")]
                EVICT_TIME.add(now.elapsed().as_nanos() as _);

                result
            }
            Policy::None => {
                #[cfg(feature = "metrics")]
                EVICT_TIME.add(now.elapsed().as_nanos() as _);

                Err(SegmentsError::NoEvictableSegments)
            }
            _ => {
                #[cfg(feature = "metrics")]
                SEGMENT_EVICT.increment();

                if let Some(id) = self.least_valuable_seg(ttl_buckets) {
                    // Resolve the segment's bucket (seg_ttl read before the
                    // claim) and lock it. LOCK: bucket-chain — the drain
                    // (finalize_drained unlink/splice) + bucket head fixup are
                    // serialized on this bucket. Capture links UNDER the lock so
                    // the head fixup is consistent with the drain. Resolving the
                    // bucket before clear_segment sidesteps the M1 ttl re-stamp.
                    let id_idx = id.get() as usize - 1;
                    let seg_ttl = self.headers[id_idx].ttl();
                    let ttl_bucket = ttl_buckets.get_bucket(seg_ttl);
                    let _chain = ttl_bucket.chain_lock();
                    let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);

                    let outcome = self.clear_segment(id, hashtable, false);

                    #[cfg(feature = "metrics")]
                    EVICT_TIME.add(now.elapsed().as_nanos() as _);

                    match outcome {
                        Err(()) => Err(SegmentsError::EvictFailure),
                        Ok(outcome) => {
                            // The segment left its chain either way.
                            if meta.prev.is_none() {
                                ttl_bucket.set_head(meta.next);
                            }
                            match outcome {
                                ClearOutcome::Freed => Ok(()),
                                // Drained and condemned, but no free
                                // segment was produced this pass.
                                ClearOutcome::Deferred => {
                                    #[cfg(feature = "metrics")]
                                    SEGMENT_PINNED_SKIP.increment();
                                    Err(SegmentsError::EvictFailure)
                                }
                            }
                        }
                    }
                } else {
                    #[cfg(feature = "metrics")]
                    {
                        SEGMENT_EVICT_EX.increment();
                        EVICT_TIME.add(now.elapsed().as_nanos() as _);
                    }

                    Err(SegmentsError::NoEvictableSegments)
                }
            }
        }
    }

    /// Returns the least valuable segment based on the configured eviction
    /// policy.
    pub(crate) fn least_valuable_seg(&self, ttl_buckets: &TtlBuckets) -> Option<NonZeroU32> {
        match self.policy {
            Policy::None => None,
            Policy::Random => {
                let mut start: u32 = self.evict.lock().unwrap().random();

                start %= self.cap;

                for i in 0..self.cap {
                    let idx = (start + i) % self.cap;
                    if self.headers[idx as usize].can_evict() {
                        // SAFETY: we are always adding 1 to the index.
                        return Some(unsafe { NonZeroU32::new_unchecked(idx + 1) });
                    }
                }

                None
            }
            Policy::RandomFifo => {
                // Pick a random accessible segment and look up the head of the
                // corresponding TtlBucket. This is equivalent to a weighted
                // random over buckets by segment count.
                let mut start: u32 = self.evict.lock().unwrap().random();

                start %= self.cap;

                for i in 0..self.cap {
                    let idx = (start + i) % self.cap;
                    if self.headers[idx as usize].state().is_readable() {
                        let ttl = self.headers[idx as usize].ttl();
                        let ttl_bucket = ttl_buckets.get_bucket(ttl);
                        return ttl_bucket.head();
                    }
                }

                None
            }
            _ => {
                if self.evict.lock().unwrap().should_rerank() {
                    self.evict.lock().unwrap().rerank(&self.headers);
                }
                loop {
                    // Bind to a local so the lock guard drops before the
                    // header read (a `while let` would hold the temporary
                    // across the body).
                    let next = self.evict.lock().unwrap().least_valuable_seg();
                    let Some(id) = next else { break };
                    // Header-only can_evict: no Segment view / &mut [u8] is
                    // derived on the un-claimed candidate (C2).
                    if self.headers[id.get() as usize - 1].can_evict() {
                        return Some(id);
                    }
                }
                None
            }
        }
    }

    // ── Remove ───────────────────────────────────────────────────────

    /// Remove a single item from a segment based on the segment id and offset.
    /// May trigger merge compaction if the merge eviction policy is active and
    /// the segment occupancy drops below the compact ratio.
    ///
    /// `pin` is a remover pin (item 7f) taken by the caller on `seg_id`
    /// BEFORE unlinking the item from the hashtable, so it brackets the
    /// unlink (caller's side) and this decrement (here) as one span a
    /// concurrent drain must wait out. It is dropped immediately after the
    /// decrement below, before any `chain_lock` acquisition — a drainer
    /// waits for `active_removers == 0` WHILE HOLDING `chain_lock`
    /// (`claim_for_drain`, run under `_chain` by `evict` and by this
    /// function's own empty-free path below), so holding the pin across
    /// that acquisition would deadlock (the same lock-order rule as item
    /// 7d's `WriterPin`).
    pub(crate) fn remove_at(
        &self,
        seg_id: NonZeroU32,
        offset: usize,
        ttl_buckets: &TtlBuckets,
        hashtable: &MultiChoiceHashtable,
        pin: RemoverPin,
    ) -> Result<(), SegmentsError> {
        // Remove the item.
        {
            let segment = self.segment(seg_id)?;
            segment.remove_item_at(offset);
            // Release the remover pin now — before any `chain_lock` below.
            drop(pin);

            // If the segment is now empty and evictable, free it
            // immediately via the drain/condemn protocol.
            if segment.live_items() == 0 && segment.can_evict() {
                // Resolve the segment's bucket (seg_ttl read before the claim)
                // and lock it. LOCK: bucket-chain — the empty-segment drain
                // (finalize_drained unlink/splice) + head fixup are serialized
                // on this bucket; resolving the bucket before clear_segment
                // sidesteps the M1 ttl re-stamp. Capture links under the lock.
                let id_idx = seg_id.get() as usize - 1;
                let seg_ttl = self.headers[id_idx].ttl();
                let ttl_bucket = ttl_buckets.get_bucket(seg_ttl);
                let _chain = ttl_bucket.chain_lock();
                let meta = segment.header_metadata();

                if self.clear_segment(seg_id, hashtable, false).is_ok() && meta.prev.is_none() {
                    ttl_bucket.set_head(meta.next);
                }
                return Ok(());
            }
        }

        // For merge eviction, check if the segment is below the compact ratio
        // low watermark. If so, perform a no-evict merge (compaction only).
        if let Policy::Merge { .. } = self.policy {
            let target_ratio = self.evict.lock().unwrap().compact_ratio();

            let id_idx = seg_id.get() as usize - 1;

            let ratio = self.headers[id_idx].live_bytes() as f64 / self.segment_size() as f64;

            if ratio > target_ratio {
                return Ok(());
            }

            if let Some(next_id) = self.headers[id_idx].next_seg() {
                let next_idx = next_id.get() as usize - 1;

                if !self.headers[next_idx].can_evict() {
                    return Ok(());
                }

                let next_ratio =
                    self.headers[next_idx].live_bytes() as f64 / self.segment_size() as f64;

                if next_ratio <= target_ratio {
                    let ttl = self.headers[id_idx].ttl();
                    let ttl_bucket = ttl_buckets.get_bucket(ttl);
                    let _ = self.merge_compact(seg_id, ttl_bucket, hashtable);
                    ttl_bucket.set_next_to_merge(None);
                }
            }
        }

        Ok(())
    }

    // ── Merge eviction ───────────────────────────────────────────────

    /// Count how many evictable segments follow `start` in the chain (up to
    /// `max_merge`).
    fn merge_evict_chain_len(&self, start: NonZeroU32) -> usize {
        let mut len = 0;
        let mut id = start;
        let max = self.evict.lock().unwrap().max_merge();

        while len < max {
            // Header-only walk: no Segment view / &mut [u8] is derived on an
            // un-claimed candidate (C2 — a concurrent evictor may be mid-copy
            // on one of these chain segments).
            let Some(hdr) = self.headers.get(id.get() as usize - 1) else {
                warn!("invalid segment id: {id}");
                break;
            };
            if hdr.can_evict() {
                len += 1;
                match hdr.next_seg() {
                    Some(i) => id = i,
                    None => break,
                }
            } else {
                break;
            }
        }

        len
    }

    /// Count how many evictable segments follow `start` whose combined live
    /// bytes fit within a single segment.
    fn merge_compact_chain_len(&self, start: NonZeroU32) -> usize {
        let mut len = 0;
        let mut id = start;
        let max = self.evict.lock().unwrap().max_merge();
        let mut occupied = 0;
        let seg_size = self.segment_size();

        while len < max {
            // Header-only walk (C2 — see merge_evict_chain_len).
            let Some(hdr) = self.headers.get(id.get() as usize - 1) else {
                warn!("invalid segment id: {id}");
                break;
            };
            if hdr.can_evict() {
                occupied += hdr.live_bytes();
                if occupied > seg_size {
                    break;
                }
                len += 1;
                match hdr.next_seg() {
                    Some(i) => id = i,
                    None => break,
                }
            } else {
                break;
            }
        }

        len
    }

    /// Merge a chain of segments starting at `start`, pruning low-frequency
    /// items and copying the survivors into a fresh spare segment. The spare
    /// is reserved from the held-back spare queue and head-inserted into
    /// `ttl_bucket` exactly once; every candidate's survivors are appended to
    /// it (reader-safe — bytes are never relocated in place) and the candidate
    /// is then drained via `clear_segment`. Returns the next segment id to
    /// merge from (if any).
    fn merge_evict(
        &self,
        start: NonZeroU32,
        ttl_bucket: &TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        #[cfg(feature = "metrics")]
        SEGMENT_MERGE.increment();

        let chain_len = self.merge_evict_chain_len(start);

        if chain_len < 3 {
            return Err(SegmentsError::NoEvictableSegments);
        }

        // Reserve the copy destination. At "full" this comes from the
        // held-back spare; if even that is empty, degrade gracefully to
        // dropping the head candidate whole rather than compacting a
        // readable segment in place.
        let spare_id = match self.reserve_spare() {
            Some(id) => id,
            None => return self.merge_evict_fallback_drop(start, ttl_bucket, hashtable),
        };

        // Configure the spare and head-insert it as `Relinking` exactly ONCE:
        // readable (so relinked survivors stay reachable) but NOT evictable, so
        // a concurrent evictor can neither select nor claim_for_drain the spare
        // while we fill it (C1). It is never the write tail (the tail is Live).
        // Because the spare is never drained, the bucket head points at it for
        // the entire candidate loop below — draining a candidate only unlinks
        // it from the middle of the chain (its neighbours are patched by
        // finalize_drained's recycle/condemn), so no per-candidate head fixup
        // is required here. `publish_dest_sealed` seals it after the loop.
        let src_ttl = self.headers[start.get() as usize - 1].ttl();
        {
            let sidx = spare_id.get() as usize - 1;
            self.headers[sidx].set_ttl(src_ttl);
            self.headers[sidx].set_pool(SegmentPool::Main);
            self.headers[sidx].mark_merged();
        }

        // LOCK: bucket-chain — serialize ALL chain-structure mutation of this
        // bucket (the spare head-insert below + each drained candidate's
        // finalize_drained unlink/splice) against concurrent evictors,
        // reservers (try_expand), and drains. Held across the copy loop
        // (coarse) so a single guard covers both the head-insert and every
        // finalize unlink — recycle/condemn/unlink take NO lock themselves, so
        // there is no re-entrant same-bucket re-lock. The reserve hot path
        // (try_alloc_item) never takes this lock; only the infrequent
        // try_expand does. Lock order: chain_lock is outer to the eviction
        // policy lock taken for the merge params below.
        let _chain = ttl_bucket.chain_lock();

        let old_head = ttl_bucket.head();
        self.link_dest_at_head(spare_id, old_head);
        ttl_bucket.set_head(Some(spare_id));

        // Merge state.
        let mut cutoff = 1.0;
        let mut merged = 0;

        // Fixed merge parameters. Read under one short-lived lock.
        let (max_merge, n_merge, stop_ratio, cfg_target_ratio) = {
            let ev = self.evict.lock().unwrap();
            (
                ev.max_merge(),
                ev.n_merge(),
                ev.stop_ratio(),
                ev.target_ratio(),
            )
        };
        let stop_bytes = (stop_ratio * self.segment_size() as f64) as i32;

        // Dynamically set target ratio based on chain length.
        let target_ratio = if chain_len < n_merge {
            1.0 / chain_len as f64
        } else {
            cfg_target_ratio
        };

        // Walk the chain, pruning each candidate and copying its survivors
        // into the spare, then draining the candidate.
        let mut next_id = Some(start);
        while let Some(cand_id) = next_id {
            if merged > max_merge {
                trace!("stop merge: merged max segments");
                break;
            }

            // Stop once the spare reaches the high-watermark occupancy.
            if self.headers[spare_id.get() as usize - 1].live_bytes() >= stop_bytes {
                trace!("stop merge: spare segment is full");
                break;
            }

            // Fast advisory pre-check: don't bother claiming a candidate that
            // isn't evictable (Live tail, already draining, or pinned past
            // can_evict). Header-only — no Segment view / &mut [u8] is derived
            // on the un-claimed candidate (C2). The authoritative claim is the
            // CAS below.
            if !self.headers[cand_id.get() as usize - 1].can_evict() {
                trace!("stop merge: can't evict candidate segment");
                break;
            }

            // Drain-first: claim the candidate's Sealed->Draining CAS BEFORE
            // mutating it. This is the uniform per-segment mutation claim for
            // ALL mutators (merge, s3fifo, drop, expire/clear), so two
            // concurrent evictors that deterministically select the same
            // candidate cannot both derive &mut to it. A lost claim means the
            // candidate was concurrently taken / is no longer Sealed — stop.
            //
            // Behavior note (spec §2): draining a candidate before copy-out
            // rejects NEW reader pins on it during the copy window (Draining is
            // not readable) and can cause a transient miss on an item mid-
            // relink (old location unpinnable, new not yet published). Existing
            // pins stay valid. Acceptable under concurrent eviction.
            if !self.claim_for_drain(cand_id) {
                trace!("stop merge: lost drain claim on candidate");
                break;
            }

            // Read next AFTER the claim (the candidate is still linked —
            // Draining does not unlink; finalize's recycle/condemn does) and
            // BEFORE finalize unlinks it.
            next_id = self.headers[cand_id.get() as usize - 1].next_seg();

            // Prune low-frequency items (marks them deleted — moves no bytes),
            // then copy the survivors into the spare. The candidate is now
            // Draining and exclusively ours. copy_into appends past the spare's
            // write-offset and republishes each survivor via the hashtable's
            // Release-CAS, so no readable segment's live bytes are ever moved
            // in place.
            {
                let mut cand = self
                    .segment(cand_id)
                    .expect("claimed chain candidate must be a valid segment id");
                let cand_old_size = cand.live_bytes();
                cutoff = cand.prune(hashtable, cutoff, target_ratio);
                trace!(
                    "cand {cand_id}: {cand_old_size} bytes -> {} bytes after prune",
                    cand.live_bytes()
                );
            }
            {
                let (mut cand, mut spare) = self
                    .segment_pair(cand_id, spare_id)
                    .expect("claimed chain candidate / own spare must be valid segment ids");
                let _ = cand.copy_into(&mut spare, hashtable);
            }

            // Finalize the already-Draining candidate (ref_count recheck +
            // recycle-or-condemn — NO second Sealed->Draining CAS). An unpinned
            // candidate is recycled, replenishing the spare via return_segment;
            // a pinned candidate is condemned to its last reader. Either way it
            // leaves the chain, and finalize's unlink patches the neighbours,
            // so the spare remains the bucket head.
            let _ = self.finalize_drained(cand_id, hashtable, false);
            merged += 1;
        }

        // Publish the filled spare: Relinking -> Sealed, making it a legal
        // future eviction candidate. Runs on every exit from the fill loop
        // (including zero candidates merged), so the spare is never left stuck
        // in Relinking (C1).
        self.publish_dest_sealed(spare_id);

        Ok(next_id)
    }

    /// Graceful degradation when no spare is available: drop the chain head
    /// whole via the drain machinery, freeing one segment (which also
    /// replenishes the spare via `return_segment` on the next unpinned
    /// recycle). No spare was head-inserted, so this path DOES fix the bucket
    /// head when the dropped segment was itself the head.
    fn merge_evict_fallback_drop(
        &self,
        start: NonZeroU32,
        ttl_bucket: &TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        // LOCK: bucket-chain — the drop drains `start` (finalize_drained's
        // unlink/splice) and fixes the bucket head; serialize that surgery on
        // this bucket. Reached only when merge_evict found no spare, BEFORE it
        // acquired the chain_lock, so there is no double-lock. Capture the
        // links under the lock so the head fixup is consistent with the drain.
        let _chain = ttl_bucket.chain_lock();
        let meta = self.headers[start.get() as usize - 1].metadata(Ordering::Acquire);
        let next = meta.next;
        match self.clear_segment(start, hashtable, false) {
            // Freed OR Deferred: the segment left service and was spliced
            // out of the chain either way (Deferred = drained + condemned to
            // AwaitingRelease, freed by its last reader's guard drop). The
            // bucket head must be advanced past it if it was the head —
            // otherwise the head dangles into a condemned/soon-reused
            // segment.
            Ok(_outcome) => {
                if meta.prev.is_none() {
                    ttl_bucket.set_head(next);
                }
                Ok(next)
            }
            // CAS failed — nothing drained, no progress.
            Err(()) => Err(SegmentsError::NoEvictableSegments),
        }
    }

    /// Merge-compact a chain of segments starting at `start` into a fresh
    /// spare, without pruning by frequency. This is best-effort maintenance
    /// invoked from `remove_at` when a segment drops below the compact-ratio
    /// low watermark — not an evict-under-pressure path — so unlike
    /// `merge_evict` it does not fall back to dropping a segment when no
    /// spare is available; it simply skips (`Ok(None)`).
    fn merge_compact(
        &self,
        start: NonZeroU32,
        ttl_bucket: &TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        #[cfg(feature = "metrics")]
        SEGMENT_MERGE.increment();

        let chain_len = self.merge_compact_chain_len(start);

        if chain_len < 2 {
            return Err(SegmentsError::NoEvictableSegments);
        }

        // Header-only read — no Segment view / &mut [u8] on the un-claimed
        // `start` (C2).
        let next_id = self.headers[start.get() as usize - 1].next_seg();

        if next_id.is_none() {
            return Err(SegmentsError::NoEvictableSegments);
        }

        // Reserve the copy destination. Compaction is best-effort
        // maintenance, not a must-free operation: if no spare is available,
        // skip rather than falling back to dropping a segment (that fallback
        // belongs to merge_evict, which MUST free a segment).
        let spare_id = match self.reserve_spare() {
            Some(id) => id,
            None => return Ok(None),
        };

        // Configure the spare and head-insert it as `Relinking` exactly ONCE:
        // same chain-head invariant as merge_evict (see its comment) — readable
        // but not evictable while we fill it, so a concurrent evictor cannot
        // select or claim_for_drain the spare (C1); `publish_dest_sealed` seals
        // it after the loop. The spare is never drained, so no per-candidate
        // head fixup is needed.
        let src_ttl = self.headers[start.get() as usize - 1].ttl();
        {
            let sidx = spare_id.get() as usize - 1;
            self.headers[sidx].set_ttl(src_ttl);
            self.headers[sidx].set_pool(SegmentPool::Main);
            self.headers[sidx].mark_merged();
        }

        // LOCK: bucket-chain — same rationale as merge_evict: one guard covers
        // the spare head-insert and every candidate's finalize unlink/splice,
        // serialized per bucket. Acquired after reserve_spare (the no-spare
        // path returns earlier without the lock). Held across the copy loop.
        let _chain = ttl_bucket.chain_lock();

        let old_head = ttl_bucket.head();
        self.link_dest_at_head(spare_id, old_head);
        ttl_bucket.set_head(Some(spare_id));

        // Merge state.
        let mut merged = 0;

        // Fixed merge parameters. Read under one short-lived lock.
        let seg_size = self.segment_size();
        let (max_merge, stop_ratio) = {
            let ev = self.evict.lock().unwrap();
            (ev.max_merge(), ev.stop_ratio())
        };
        let stop_bytes = (stop_ratio * self.segment_size() as f64) as i32;

        // Walk the chain, copying each candidate's survivors into the spare
        // (no prune — merge_compact combines under-full segments as-is,
        // without frequency-based pruning), then draining the candidate.
        let mut next_id = Some(start);
        while let Some(cand_id) = next_id {
            if merged > max_merge {
                trace!("stop merge: merged max segments");
                break;
            }

            // Fast advisory pre-check (header-only — no Segment view / &mut [u8]
            // on the un-claimed candidate, C2; authoritative claim is the CAS
            // below).
            if !self.headers[cand_id.get() as usize - 1].can_evict() {
                trace!("stop merge: can't evict candidate segment");
                break;
            }

            let spare_size = self.headers[spare_id.get() as usize - 1].live_bytes();
            let cand_size = self.headers[cand_id.get() as usize - 1].live_bytes();

            if spare_size >= stop_bytes || spare_size + cand_size > seg_size {
                trace!("stop merge: spare segment is full");
                break;
            }

            // Drain-first: claim the candidate's Sealed->Draining CAS BEFORE
            // copying it out — the same uniform per-segment mutation claim as
            // merge_evict (see its comment for the full rationale + the
            // transient-miss behavior note). A lost claim means the candidate
            // was concurrently taken / is no longer Sealed — stop.
            if !self.claim_for_drain(cand_id) {
                trace!("stop merge: lost drain claim on candidate");
                break;
            }

            // Read next AFTER the claim (Draining does not unlink) and BEFORE
            // finalize unlinks it.
            next_id = self.headers[cand_id.get() as usize - 1].next_seg();

            // Copy the survivors into the spare. The candidate is now Draining
            // and exclusively ours. copy_into appends past the spare's write-
            // offset and republishes each survivor via the hashtable's
            // Release-CAS, so no readable segment's live bytes are ever moved
            // in place.
            {
                let (mut cand, mut spare) = self
                    .segment_pair(cand_id, spare_id)
                    .expect("claimed chain candidate / own spare must be valid segment ids");
                let _ = cand.copy_into(&mut spare, hashtable);
            }

            // Finalize the already-Draining candidate (ref_count recheck +
            // recycle-or-condemn — NO second Sealed->Draining CAS). An unpinned
            // candidate is recycled, replenishing the spare via return_segment;
            // a pinned candidate is condemned to its last reader. Either way it
            // leaves the chain, and finalize's unlink patches the neighbours,
            // so the spare remains the bucket head.
            let _ = self.finalize_drained(cand_id, hashtable, false);
            merged += 1;
        }

        // Publish the filled spare: Relinking -> Sealed (see merge_evict). Runs
        // on every exit from the fill loop, so the spare is never left stuck in
        // Relinking (C1).
        self.publish_dest_sealed(spare_id);

        Ok(next_id)
    }

    // ── S3-FIFO eviction ─────────────────────────────────────────────

    /// Find the oldest evictable segment in the given pool across all TTL
    /// buckets.
    fn find_oldest_seg_in_pool(
        &self,
        ttl_buckets: &TtlBuckets,
        pool: SegmentPool,
    ) -> Option<NonZeroU32> {
        let mut best: Option<(NonZeroU32, Instant)> = None;

        for bucket in &ttl_buckets.buckets[..] {
            let mut id_opt = bucket.head();
            while let Some(id) = id_opt {
                let hdr = &self.headers[id.get() as usize - 1];
                if hdr.pool() == pool && hdr.can_evict() {
                    let age = std::cmp::max(hdr.create_at(), hdr.merge_at());
                    if best.is_none() || age < best.unwrap().1 {
                        best = Some((id, age));
                    }
                }
                id_opt = hdr.next_seg();
            }
        }

        best.map(|(id, _)| id)
    }

    /// S3-FIFO eviction entry point. Tries admission pool first (the
    /// filtering step), then main pool (CLOCK second-chance).
    fn s3fifo_evict(
        &self,
        ttl_buckets: &TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Try evicting an admission-pool segment first (promoting freq > 0).
        if let Some(seg_id) = self.find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Admission) {
            return self.s3fifo_evict_admission(seg_id, ttl_buckets, hashtable);
        }

        // No admission-pool segments evictable; try main pool.
        if let Some(seg_id) = self.find_oldest_seg_in_pool(ttl_buckets, SegmentPool::Main) {
            return self.s3fifo_evict_main(seg_id, ttl_buckets, hashtable);
        }

        #[cfg(feature = "metrics")]
        SEGMENT_EVICT_EX.increment();

        Err(SegmentsError::NoEvictableSegments)
    }

    /// Evict an admission-pool segment. Items with freq > 0 are promoted
    /// (copied to a main-pool segment). Items with freq == 0 are dropped
    /// and their key hashes are added to the ghost queue.
    fn s3fifo_evict_admission(
        &self,
        seg_id: NonZeroU32,
        ttl_buckets: &TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Resolve the source's bucket (source and promotion target share
        // src_ttl's bucket — Bs == Bt) and lock it BEFORE claiming the source.
        // LOCK: bucket-chain — one guard covers the source claim, the target
        // head-insert, the source finalize unlink/splice, and the head fixup;
        // Bs == Bt so no two bucket locks are ever held at once. Held across the
        // promote copy (coarse, single guard); claim_for_drain / finalize
        // primitives take no lock, so no re-entrant same-bucket re-lock.
        // Resolving the bucket from the pre-claim ttl is safe: on a lost claim
        // nothing is mutated (no target reserved yet), and if the source's ttl
        // changed under us the claim CAS simply fails.
        let id_idx = seg_id.get() as usize - 1;
        let src_ttl = self.headers[id_idx].ttl();
        let ttl_bucket = ttl_buckets.get_bucket(src_ttl);
        let _chain = ttl_bucket.chain_lock();

        // Drain-first: claim the source's Sealed->Draining CAS BEFORE promoting
        // items out of it (s3fifo_promote_from calls remove_item_at on src).
        // The claim is taken UNDER the chain_lock — symmetric with every other
        // evict/drain path (merge per-candidate, evict-default, remove_at,
        // drain_chain) — so a bucket-lock holder (e.g. a concurrent
        // drain_chain/expire) never observes this source mid-claim in Draining
        // (which would trip drain_chain's Sealed/Live debug_assert). A lost
        // claim means the source was already taken by another mutator — fail
        // this pass, having mutated nothing.
        //
        // Behavior note (spec §2): draining the source before copy-out rejects
        // NEW reader pins on it during the promotion window and can cause a
        // transient miss on an item mid-relink; existing pins stay valid.
        // Acceptable under concurrent eviction.
        if !self.claim_for_drain(seg_id) {
            return Err(SegmentsError::EvictFailure);
        }

        // First pass: copy items with freq > 0 into a main-pool segment. The
        // source is now Draining and exclusively ours.
        let target_id = self.reserve_free();

        if let Some(tid) = target_id {
            self.headers[tid.get() as usize - 1].set_pool(SegmentPool::Main);
            self.headers[tid.get() as usize - 1].set_ttl(src_ttl);
            // Link the target at the head of the TTL bucket, published as
            // `Relinking`: readable (promoted survivors stay reachable) but NOT
            // evictable, so a concurrent evictor can neither select nor
            // claim_for_drain the target while we fill it (C1). It is never the
            // write tail (Live == the bucket tail reserve() writes into).
            let old_head = ttl_bucket.head();
            self.link_dest_at_head(tid, old_head);
            ttl_bucket.set_head(Some(tid));

            self.s3fifo_promote_from(seg_id, tid, hashtable);

            // Publish the filled target: Relinking -> Sealed, making it a legal
            // future eviction candidate (C1).
            self.publish_dest_sealed(tid);
        }
        // If no free segment, we just drop everything (all items evicted).

        // Add hashes of remaining (freq == 0) items to ghost queue.
        self.s3fifo_ghost_remaining(seg_id, hashtable);

        // Finalize the already-Draining source (recycle-or-condemn, NO second
        // Sealed->Draining CAS). Capture links AFTER link_dest_at_head patched
        // the source's prev (if the source was the old head, its prev now
        // points at the freshly linked target) and BEFORE finalize unlinks it.
        let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);
        let outcome = self.finalize_drained(seg_id, hashtable, false);

        // The segment left its chain either way.
        if meta.prev.is_none() {
            ttl_bucket.set_head(meta.next);
        }

        match outcome {
            ClearOutcome::Freed => Ok(()),
            ClearOutcome::Deferred => {
                #[cfg(feature = "metrics")]
                SEGMENT_PINNED_SKIP.increment();
                Err(SegmentsError::EvictFailure)
            }
        }
    }

    /// Copy items with freq > 0 from src to dst (promotion).
    fn s3fifo_promote_from(
        &self,
        src_id: NonZeroU32,
        dst_id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
    ) {
        let seg_size = self.segment_size() as usize;
        let (src, dst) = match self.segment_pair(src_id, dst_id) {
            Ok(pair) => pair,
            Err(_) => return,
        };

        let max_offset = src.max_item_offset();
        let mut offset = if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        };

        while offset <= max_offset {
            let item = match src.get_item_at(offset) {
                Some(i) => i,
                None => break,
            };
            if item.klen() == 0 && src.live_items() == 0 {
                break;
            }
            item.check_magic();

            let item_size = item.size();
            let old_loc = pack_location(src.id(), offset as u64);
            if item.is_deleted() {
                offset += item_size;
                continue;
            }
            let freq = hashtable
                .get_item_frequency(item.key(), old_loc)
                .unwrap_or(0);
            if freq == 0 {
                offset += item_size;
                continue;
            }

            if freq > 0 {
                let write_offset = dst.write_offset() as usize;
                if write_offset + item_size < seg_size {
                    let new_loc = pack_location(dst.id(), write_offset as u64);
                    // NUMERIC RELOCATION GATE (see `Segment::copy_into` for
                    // the full argument): hold the item's seqlock writer
                    // lock across the byte copy AND the relink CAS so an
                    // in-place numeric writer (reader-pinned only — the
                    // drain claim does not exclude it) can neither tear the
                    // copy, leak an acked increment into the orphaned
                    // source, nor have its transient odd version published.
                    // The destination is stamped back to the frozen even
                    // version before the publish; the lock is a leaf, so no
                    // cycle is added.
                    let vguard = item.lock_numeric_version().ok();
                    // Copy-then-publish (see copy_into): write bytes before the
                    // Release-CAS publishes new_loc. On CAS failure the bytes are
                    // orphaned (write_offset not advanced) and the item stays in
                    // src to be evicted — same outcome as before, minus the
                    // torn-read window.
                    let d = unsafe {
                        let s = src.data_ptr().add(offset);
                        let d = dst.data_ptr().add(write_offset);
                        std::ptr::copy_nonoverlapping(s, d, item_size);
                        d
                    };
                    if let Some(guard) = &vguard {
                        guard.stamp_relocated_copy(&RawItem::from_ptr(d));
                    }
                    let relinked = hashtable.cas_location(item.key(), old_loc, new_loc, true);
                    // Unlock only AFTER the publish resolved (or failed), so
                    // a spinning numeric writer's in-lock re-validation sees
                    // the outcome.
                    drop(vguard);
                    if relinked {
                        src.remove_item_at(offset);
                        // A promotion is not a death: take back the dead
                        // charge `remove_item_at` just put on the source
                        // (see `Segment::copy_into`). Unconditional, like
                        // every other header counter — only the global gauge
                        // mirror below is `metrics`-gated.
                        src.decr_dead_item(item_size as i32);
                        dst.incr_live_items();
                        dst.incr_live_bytes(item_size as i32);
                        dst.set_write_offset(write_offset as i32 + item_size as i32);

                        #[cfg(feature = "metrics")]
                        {
                            ITEM_RELINK.increment();
                            ITEM_COMPACTED.increment();
                            // A promotion MOVES an item, it does not kill
                            // one, so it must be gauge-NEUTRAL on BOTH
                            // sides: the `remove_item_at` above decremented
                            // the global live gauges and incremented the
                            // global dead gauges, while the destination's
                            // header bumps do not touch either. Undo here,
                            // per item, exactly as `Segment::copy_into`
                            // does.
                            ITEM_CURRENT.increment();
                            ITEM_CURRENT_BYTES.add(item_size as _);
                            ITEM_DEAD.decrement();
                            ITEM_DEAD_BYTES.sub(item_size as _);
                        }
                    }
                }
                // If no room in target, item stays in source and will be evicted.
            }

            offset += item_size;
        }
    }

    /// Add hashes of remaining live items in a segment to the ghost queue.
    fn s3fifo_ghost_remaining(&self, seg_id: NonZeroU32, hashtable: &MultiChoiceHashtable) {
        // Collect hashes first to avoid holding the segment view across the
        // eviction lock.
        let mut hashes = Vec::new();
        {
            let segment = match self.segment(seg_id) {
                Ok(s) => s,
                Err(_) => return,
            };

            let max_offset = segment.max_item_offset();
            let mut offset = if cfg!(feature = "integrity") {
                std::mem::size_of_val(&SEG_MAGIC)
            } else {
                0
            };

            while offset <= max_offset {
                let item = match segment.get_item_at(offset) {
                    Some(i) => i,
                    None => break,
                };
                if item.klen() == 0 {
                    break;
                }

                let item_size = item.size();
                if !item.is_deleted() {
                    let loc = pack_location(segment.id(), offset as u64);
                    let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();
                    if !deleted {
                        let mut hasher = hashtable.hash_builder().build_hasher();
                        hasher.write(item.key());
                        hashes.push(hasher.finish());
                    }
                }

                offset += item_size;
            }
        }

        if !hashes.is_empty() {
            let mut ev = self.evict.lock().unwrap();
            for hash in hashes {
                ev.ghost.insert(hash);
            }
        }
    }

    /// Evict a main-pool segment using CLOCK-style second chance. Items with
    /// freq > 0 are copied to a fresh main segment. Items with freq == 0 are
    /// dropped.
    fn s3fifo_evict_main(
        &self,
        seg_id: NonZeroU32,
        ttl_buckets: &TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // LOCK: bucket-chain — same structure as s3fifo_evict_admission: resolve
        // the source's bucket (Bs == Bt, both src_ttl's bucket) and lock it
        // BEFORE claiming the source; the guard covers the source claim, the
        // target head-insert, the source finalize unlink/splice, and the head
        // fixup. No two bucket locks held at once. Held across the promote copy
        // (coarse, single guard).
        let id_idx = seg_id.get() as usize - 1;
        let src_ttl = self.headers[id_idx].ttl();
        let ttl_bucket = ttl_buckets.get_bucket(src_ttl);
        let _chain = ttl_bucket.chain_lock();

        // Drain-first: claim the source's Sealed->Draining CAS UNDER the
        // chain_lock (symmetric with every other evict/drain path — see
        // s3fifo_evict_admission for the full rationale + behavior note), so a
        // concurrent bucket-lock holder never observes this source mid-claim in
        // Draining. A lost claim fails this pass, having mutated nothing.
        if !self.claim_for_drain(seg_id) {
            return Err(SegmentsError::EvictFailure);
        }

        // Try to get a target segment for second-chance items. The source is
        // now Draining and exclusively ours.
        let target_id = self.reserve_free();

        if let Some(tid) = target_id {
            self.headers[tid.get() as usize - 1].set_pool(SegmentPool::Main);
            self.headers[tid.get() as usize - 1].set_ttl(src_ttl);
            // Head insert as `Relinking`, then seal after the fill (see
            // s3fifo_evict_admission for the C1 rationale).
            let old_head = ttl_bucket.head();
            self.link_dest_at_head(tid, old_head);
            ttl_bucket.set_head(Some(tid));

            // Copy freq > 0 items (same promote logic, but no ghost).
            self.s3fifo_promote_from(seg_id, tid, hashtable);

            // Publish the filled target: Relinking -> Sealed (C1).
            self.publish_dest_sealed(tid);
        }

        // Finalize the already-Draining source (recycle-or-condemn, NO second
        // Sealed->Draining CAS). Capture links after link_dest_at_head, before
        // finalize unlinks it.
        let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);
        let outcome = self.finalize_drained(seg_id, hashtable, false);

        // The segment left its chain either way.
        if meta.prev.is_none() {
            ttl_bucket.set_head(meta.next);
        }

        match outcome {
            ClearOutcome::Freed => Ok(()),
            ClearOutcome::Deferred => {
                #[cfg(feature = "metrics")]
                SEGMENT_PINNED_SKIP.increment();
                Err(SegmentsError::EvictFailure)
            }
        }
    }

    // ── Ghost queue ──────────────────────────────────────────────────

    /// Check if a key hash is in the ghost queue (S3-FIFO). Locks the
    /// eviction mutex internally. Called from `reserve_and_define` OUTSIDE
    /// eviction, so it never nests with an eviction-held lock in one thread.
    pub(crate) fn ghost_contains(&self, hash: u64) -> bool {
        self.evict.lock().unwrap().ghost.contains(hash)
    }

    /// Remove a hash from the ghost queue (on ghost hit). Locks the eviction
    /// mutex internally; see `ghost_contains` on lock nesting.
    pub(crate) fn ghost_remove(&self, hash: u64) {
        self.evict.lock().unwrap().ghost.remove(hash);
    }

    // ── Debug / test helpers ─────────────────────────────────────────

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

    /// Print all segment headers to stdout.
    #[cfg(test)]
    pub(crate) fn print_headers(&self) {
        for id in 0..self.cap {
            println!("segment header: {:?}", self.headers[id as usize]);
        }
    }

    /// Verify that every segment's counted live items match its header.
    #[cfg(feature = "debug")]
    pub(crate) fn check_integrity(&self, hashtable: &MultiChoiceHashtable) -> bool {
        let mut integrity = true;
        for id in 0..self.cap {
            let idx = id as usize;
            let seg_start = self.segment_size as usize * idx;
            let seg_end = seg_start + self.segment_size as usize;
            let header = &self.headers[idx];
            // Only in-service segments (Live/Sealed/Relinking) have a
            // meaningful counted-vs-header comparison. A segment on its way
            // out can legitimately carry residual counters from
            // unlinked-without-pin removals — a `Draining` one is mid-parse
            // by its owner, and an `AwaitingRelease` one is waiting on its
            // last pin to drop — so counting either would report a false
            // mismatch. The residue is settled by the `reset_write_stats`
            // on whichever path actually frees the segment, so a `Free` one
            // no longer carries it.
            match header.state() {
                State::Live | State::Sealed | State::Relinking => {}
                _ => continue,
            }
            // SAFETY: we only read the data here; the borrow is scoped.
            let data = unsafe {
                std::slice::from_raw_parts_mut(self.data.as_ptr() as *mut u8, self.data.len())
            };
            let segment = Segment::from_raw_parts(header, &mut data[seg_start..seg_end]);
            if !segment.check_integrity(hashtable) {
                integrity = false;
            }
        }
        integrity
    }
}

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
        let segments = build(
            Policy::Merge {
                max: 8,
                merge: 4,
                compact: 0,
            },
            16,
        );
        // 16 total: 1 spare + 15 free.
        assert_eq!(segments.spare_capacity(), 1);
        assert_eq!(segments.free(), 16, "free() counts free + spare");
        assert_eq!(
            segments.free_only(),
            15,
            "normal free queue excludes the spare"
        );
    }

    #[test]
    fn non_merge_policy_holds_back_no_spare() {
        let segments = build(Policy::Random, 16);
        assert_eq!(segments.spare_capacity(), 0);
        assert_eq!(segments.free_only(), 16);
    }

    #[test]
    fn reserve_spare_prefers_spare_then_falls_back_to_free() {
        let segments = build(
            Policy::Merge {
                max: 8,
                merge: 4,
                compact: 0,
            },
            4,
        );
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
        assert_eq!(
            segments.spare_count(),
            1,
            "return replenished the spare, not the free queue"
        );
        assert_eq!(
            segments.free_only(),
            0,
            "the returned segment replenished the spare, not the free queue"
        );
    }

    #[test]
    fn reserve_spare_falls_back_to_free_when_spare_empty() {
        let segments = build(
            Policy::Merge {
                max: 8,
                merge: 4,
                compact: 0,
            },
            4,
        );
        // 4 total: 1 spare + 3 free.
        assert_eq!(segments.free_only(), 3);

        // Reserve the spare directly (not via reserve_free): the normal
        // free queue must be untouched.
        let from_spare = segments.reserve_spare().expect("spare available");
        assert_eq!(
            segments.free_only(),
            3,
            "reserving the spare must not touch the free queue"
        );

        // The spare is now empty: the next reserve_spare must hit
        // Steal::Empty and fall back to reserve_free, returning Some and
        // pulling one segment out of the normal free queue.
        let from_free = segments
            .reserve_spare()
            .expect("reserve_spare must fall back to the free queue when the spare is empty");
        assert_eq!(
            segments.free_only(),
            2,
            "the fallback reservation must come from the free queue"
        );
        assert_ne!(
            from_spare, from_free,
            "spare and fallback must be distinct segments"
        );
    }

    // Roadmap item 5b: the no-spare merge fallback drops the chain head via
    // clear_segment. When that head is pinned by a live reader, clear_segment
    // condemns it (Deferred / AwaitingRelease) instead of freeing it — the
    // segment still leaves its chain. The fallback MUST advance the bucket
    // head past the condemned segment; leaving the head pointing at a
    // condemned (and soon-reused) segment is the latent bug this guards.
    //
    // This branch is unreachable through the normal evict() path today (the
    // chain-length guard requires an unpinned head and &mut-serialization
    // keeps it unpinned through clear_segment), so the fallback is driven
    // directly with a manually pinned head — like the item-4 concurrency
    // tests. Uses test-only accessors, so it is gated with the module.
    #[test]
    fn merge_evict_fallback_drop_fixes_head_on_condemned_segment() {
        use crate::Segcache;
        use core::num::NonZeroU32;
        use std::time::Duration;

        const ITEMS_PER_SEGMENT: usize = 4;
        const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
        let value: &[u8] = b"x";
        let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
        let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

        // 1 held-back spare (Merge policy) + 4 free.
        let total_segments = 5usize;

        let cache = Segcache::builder()
            .segment_size(segment_size)
            .heap_size(segment_size as usize * total_segments)
            .hash_power(16)
            .eviction(Policy::Merge {
                max: 8,
                merge: 4,
                compact: 0,
            })
            .build()
            .expect("failed to create cache");

        // Long TTL so nothing expires; all items share one bucket.
        let ttl = Duration::from_secs(3600);

        // Fill enough to span at least two segments so the bucket head
        // (segment id 2 — id 1 is the held-back spare) is Sealed with a
        // next. Ten items across 4-item segments -> seg2, seg3, seg4.
        for i in 0..10 {
            let key = format!("k{i:06}");
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), value, None, ttl)
                .expect("fill inserts must succeed");
        }

        // The head of the chain is the oldest (first-reserved) segment, id 2.
        let head = NonZeroU32::new(2).unwrap();
        assert_eq!(cache.segments.header(head).state(), State::Sealed);
        let next = cache.segments.header(head).metadata(Ordering::Acquire).next;
        assert!(next.is_some(), "head must have a successor in the chain");
        let seg_ttl = cache.segments.header(head).ttl();
        assert_eq!(
            cache.ttl_buckets.get_bucket(seg_ttl).head(),
            Some(head),
            "precondition: bucket head is the segment we drop"
        );

        // Pin the head segment with a live reader: the first inserted key
        // lives in seg 2. Holding the Item keeps its SegmentGuard alive.
        let held = cache.get(b"k000000").expect("head item must resolve");
        assert!(
            cache.segments.header(head).ref_count() > 0,
            "the held item must pin the head segment"
        );

        // Drive the no-spare fallback directly.
        let res = {
            let bucket = cache.ttl_buckets.get_bucket(seg_ttl);
            cache
                .segments
                .merge_evict_fallback_drop(head, bucket, &cache.hashtable)
        };

        // (a) The fallback made progress and returned the old head's next.
        assert!(
            matches!(res, Ok(n) if n == next),
            "fallback must return Ok(next), got {res:?}"
        );
        // (b) The pinned head was condemned (drained + spliced out), not freed.
        assert_eq!(
            cache.segments.header(head).state(),
            State::AwaitingRelease,
            "a pinned head must be condemned, not freed"
        );
        // (c) The bucket head advanced past the condemned segment — the fix.
        assert_eq!(
            cache.ttl_buckets.get_bucket(seg_ttl).head(),
            next,
            "bucket head must advance to the old head's next, never dangle \
             into the condemned segment"
        );

        // The pinned reader still reads intact bytes while condemned.
        assert_eq!(held.value(), b"x");

        // Dropping the guard completes the AwaitingRelease handoff: the
        // condemned segment returns to the pool (no leak).
        let free_before = cache.segments.free();
        drop(held);
        assert_eq!(
            cache.segments.header(head).state(),
            State::Free,
            "guard drop must free the condemned segment"
        );
        assert_eq!(
            cache.segments.free(),
            free_before + 1,
            "no leak: the condemned segment returns to the pool"
        );
    }

    // ISSUE #64, DEFECT 1 — deterministic reproduction.
    //
    // `AwaitingRelease` is readable, so a reader that resolved a location
    // BEFORE the drain and then stalled can take a NEW pin after the
    // condemn. The reader count therefore returns to non-zero after
    // reaching zero, and the last reader's guard drop — which decided it
    // was last from `prev == 1` before the new pin landed — frees the
    // segment out from under that live pin.
    //
    // The interleaving is driven by hand rather than by threads: the guard
    // drop is exactly `release_reader_for_guard()` followed by
    // `try_release_condemned()`, so leaking the Item with `mem::forget`
    // and calling those two halves separately places the stalled reader's
    // pin precisely in the gap between them. No scheduler involved.
    #[test]
    fn condemned_segment_is_freed_under_a_live_reader_pin() {
        use crate::Segcache;
        use core::num::NonZeroU32;
        use std::time::Duration;

        const ITEMS_PER_SEGMENT: usize = 4;
        const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
        let value: &[u8] = b"x";
        let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
        let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
        let total_segments = 5usize;

        let cache = Segcache::builder()
            .segment_size(segment_size)
            .heap_size(segment_size as usize * total_segments)
            .hash_power(16)
            .eviction(Policy::Merge {
                max: 8,
                merge: 4,
                compact: 0,
            })
            .build()
            .expect("failed to create cache");

        let ttl = Duration::from_secs(3600);
        for i in 0..10 {
            let key = format!("k{i:06}");
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), value, None, ttl)
                .expect("fill inserts must succeed");
        }

        // Segment 1 is the held-back merge spare; segment 2 is the chain
        // head and holds the first key.
        let head = NonZeroU32::new(2).unwrap();
        assert_eq!(cache.segments.header(head).state(), State::Sealed);

        // Reader B's stalled lookup: it has already resolved `k000000` to
        // (head, first-item offset) and has NOT yet called
        // `acquire_item_at`. This is the location it will pin with.
        let stalled_offset = magic_overhead;

        // Reader A holds a live pin via the Item's SegmentGuard.
        let a = cache.get(b"k000000").expect("head item must resolve");
        assert_eq!(a.value(), b"x");
        assert_eq!(cache.segments.header(head).ref_count(), 1);

        // Drain and condemn the head while A pins it.
        assert!(cache.segments.claim_for_drain_for_test(head));
        assert_eq!(
            cache
                .segments
                .finalize_drained_for_test(head, &cache.hashtable),
            ClearOutcome::Deferred,
            "a pinned segment must be condemned, not recycled"
        );
        assert_eq!(cache.segments.header(head).state(), State::AwaitingRelease);
        // The hashtable is fully drained: no NEW lookup can route here.
        assert!(cache.get(b"k000000").is_none());

        // --- A's guard drop, phase 1: the SeqCst decrement. Count -> 0.
        std::mem::forget(a);
        let prev = cache.segments.header(head).release_reader_for_guard();
        assert_eq!(prev, 1, "A must observe itself as the last reader");
        assert_eq!(cache.segments.header(head).ref_count(), 0);

        // --- B resumes HERE, in the gap between A's decrement and A's
        // release CAS, and pins with its pre-drain location.
        let pinned = cache.segments.acquire_item_at(head, stalled_offset);
        let b_pinned = pinned.is_some();
        eprintln!(
            "B's post-condemn pin: {} (ref_count now {})",
            if b_pinned { "SUCCEEDED" } else { "failed" },
            cache.segments.header(head).ref_count()
        );

        // --- A's guard drop, phase 2: the release CAS.
        let won = cache.segments.header(head).try_release_condemned();
        assert!(won, "A wins the AwaitingRelease -> Free transition");
        assert_eq!(cache.segments.header(head).state(), State::Free);
        eprintln!(
            "after A's release CAS: state={:?} ref_count={}",
            cache.segments.header(head).state(),
            cache.segments.header(head).ref_count()
        );

        // The next incarnation: reserve the segment back out of the pool
        // while B's pin is still outstanding. (The free queue is FIFO, so
        // pull until this id comes back around.)
        cache.segments.free_queue.push(head.get());
        let mut reused = None;
        for _ in 0..16 {
            match cache.segments.reserve_free() {
                Some(id) if id == head => {
                    reused = Some(id);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        let reused = reused.expect("the freed segment must be reservable again");
        let phantom = cache.segments.header(reused).ref_count();
        eprintln!(
            "next incarnation of segment {reused}: state={:?} ref_count={phantom}",
            cache.segments.header(reused).state(),
        );

        assert!(
            !b_pinned,
            "a NEW reader pin must not succeed on a condemned segment"
        );
        assert_eq!(
            phantom, 0,
            "the next incarnation must not inherit a phantom reader count"
        );
        drop(pinned);
    }

    // ISSUE #64, DEFECT 3 — deterministic reproduction.
    //
    // `try_acquire_reader` increments `ref_count` BEFORE it knows the
    // segment is still readable. A drain that runs inside that window sees
    // the transient pin, so `finalize_drained` condemns the segment and
    // defers reclamation to "the last reader" — but the only reader is the
    // acquire itself, which is about to back its pin out. A backout that
    // performs no handoff leaves the segment in `AwaitingRelease` with
    // nobody left to free it, and nothing anywhere sweeps that state: the
    // strand is permanent, costing a segment per occurrence.
    //
    // Driven through the acquire's interposition hook rather than by
    // threads, so the drain lands exactly in the increment/re-check window
    // with no scheduler involved. The call under test is the production
    // `acquire_item_at`, so the free-queue return on the winning backout is
    // covered too, not just the state transition.
    #[test]
    fn acquire_backout_must_not_strand_a_condemned_segment() {
        use crate::segments::header::acquire_hook;
        use crate::Segcache;
        use core::num::NonZeroU32;
        use std::rc::Rc;
        use std::time::Duration;

        const ITEMS_PER_SEGMENT: usize = 4;
        const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
        let value: &[u8] = b"x";
        let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(value), 0);
        let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
        let total_segments = 5usize;

        // `Rc` only so the hook closure can own a handle to the cache; the
        // whole test runs on one thread.
        let cache = Rc::new(
            Segcache::builder()
                .segment_size(segment_size)
                .heap_size(segment_size as usize * total_segments)
                .hash_power(16)
                .eviction(Policy::Merge {
                    max: 8,
                    merge: 4,
                    compact: 0,
                })
                .build()
                .expect("failed to create cache"),
        );

        let ttl = Duration::from_secs(3600);
        for i in 0..10 {
            let key = format!("k{i:06}");
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), value, None, ttl)
                .expect("fill inserts must succeed");
        }

        // Segment 1 is the held-back merge spare; segment 2 is the chain
        // head and holds the first key.
        let head = NonZeroU32::new(2).unwrap();
        assert_eq!(cache.segments.header(head).state(), State::Sealed);
        assert_eq!(
            cache.segments.header(head).ref_count(),
            0,
            "precondition: no real reader pins the head"
        );
        let free_before = cache.segments.free();

        // Phase 0: the acquire has taken its transient pin and has not yet
        //   re-checked. A drain claims the segment here, so the re-check
        //   will see `Draining` and the acquire will back out.
        // Phase 1: the acquire is parked between that failed re-check and
        //   its backout. The drain finishes: it observes the transient pin
        //   and condemns, deferring reclamation to "the last reader".

        // Read inside the hook, at the instant the segment becomes
        // AwaitingRelease: the dead charge the backout is then on the hook
        // to settle.
        let dead_at_condemn = Rc::new(std::cell::Cell::new(0i32));

        let hooked = Rc::clone(&cache);
        let dead_probe = Rc::clone(&dead_at_condemn);
        let hook = acquire_hook::install(Box::new(move |phase| match phase {
            0 => {
                assert!(
                    hooked.segments.claim_for_drain_for_test(head),
                    "the drain must win the Sealed -> Draining claim"
                );
            }
            _ => {
                assert_eq!(
                    hooked
                        .segments
                        .finalize_drained_for_test(head, &hooked.hashtable),
                    ClearOutcome::Deferred,
                    "the drain observes the transient pin and condemns"
                );
                assert_eq!(
                    hooked.segments.header(head).state(),
                    State::AwaitingRelease,
                    "the drain handed reclamation to the last reader"
                );
                dead_probe.set(hooked.segments.header(head).dead_items());
            }
        }));

        let acquired = cache.segments.acquire_item_at(head, magic_overhead);
        drop(hook);

        assert!(
            acquired.is_none(),
            "the acquire must fail on a segment claimed out from under it"
        );
        assert_eq!(
            cache.segments.header(head).ref_count(),
            0,
            "the backout must leave no pin behind"
        );
        assert_eq!(
            cache.segments.header(head).state(),
            State::Free,
            "the backout removed the LAST pin on a condemned segment, so it \
             owes the AwaitingRelease -> Free handoff; nothing else will ever \
             do it and nothing sweeps AwaitingRelease"
        );
        assert_eq!(
            cache.segments.free(),
            free_before + 1,
            "the segment must return to the pool, not be stranded"
        );

        // The winning backout is the THIRD claimant of the AwaitingRelease
        // -> Free CAS, alongside the last guard drop and the condemner's
        // race-fix recheck, so it owes the same accounting settlement they
        // do. A segment must never land on the free queue still carrying
        // the dead weight of everything that died in it, nor the live-side
        // residue of removals that unlinked without a remover pin: until
        // some later `try_reserve` happens to pick that segment up,
        // `ITEM_DEAD`/`ITEM_DEAD_BYTES` sit above the true dead occupancy
        // and `ITEM_CURRENT`/`ITEM_CURRENT_BYTES` above the true live count
        // (issue #58 part 2). For a cache that stops writing, "later" is
        // never.
        let freed = cache.segments.header(head);
        // Non-vacuity: the drain really did kill items in this segment, so
        // there was a charge to settle.
        assert!(
            dead_at_condemn.get() > 0,
            "the drain must have charged the segment with dead space, or \
             this probe proves nothing"
        );
        let initial_offset = if cfg!(feature = "integrity") {
            core::mem::size_of::<u64>() as i32
        } else {
            0
        };
        assert_eq!(
            (
                freed.dead_items(),
                freed.dead_bytes(),
                freed.live_items(),
                freed.live_bytes(),
            ),
            (0, 0, 0, initial_offset),
            "(dead_items, dead_bytes, live_items, live_bytes) — the backout \
             freed the segment without settling its accounting, so it reached \
             the free queue still charged"
        );
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::sync::atomic::AtomicU32 as LoomAtomicU32;
    use loom::sync::atomic::Ordering;
    use loom::sync::Arc;
    use loom::thread;

    // The return_segment spare replenishment: N threads race to bump
    // spare_count from below capacity to at most capacity. The CAS ensures
    // at most `capacity` returners "win" the spare (bump the count); the
    // rest fall through to the free queue. Models the exact CAS logic of
    // Segments::return_segment. SC-independent CAS-uniqueness (like the
    // item-4 election models) — loom-provable.
    #[test]
    fn loom_return_segment_no_overfill() {
        loom::model(|| {
            const CAPACITY: u32 = 1;
            let spare_count = Arc::new(LoomAtomicU32::new(0));

            // Two returners race (guard-drop vs evictor recycle).
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let sc = spare_count.clone();
                    thread::spawn(move || {
                        // mirror return_segment's CAS loop; return true if it
                        // "won" a spare slot (would push to spare_queue).
                        let mut count = sc.load(Ordering::Relaxed);
                        loop {
                            if count >= CAPACITY {
                                break false;
                            }
                            match sc.compare_exchange_weak(
                                count,
                                count + 1,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => break true,
                                Err(observed) => count = observed,
                            }
                        }
                    })
                })
                .collect();
            let wins: u32 = handles.into_iter().map(|h| h.join().unwrap() as u32).sum();

            // At most CAPACITY returners win the spare; count never overfills.
            assert!(
                wins <= CAPACITY,
                "spare overfilled: {wins} wins > {CAPACITY}"
            );
            assert_eq!(spare_count.load(Ordering::Relaxed), wins);
            assert!(spare_count.load(Ordering::Relaxed) <= CAPACITY);
        });
    }
}
