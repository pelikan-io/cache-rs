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
    /// Current spare-queue depth. Single-writer today — eviction (the
    /// only caller of `reserve_spare`/`return_segment`) is `&mut`-
    /// serialized, so the check-then-act in `return_segment` is race-free
    /// in practice. This is `Atomic` for the type to stay stable, but a
    /// CAS/bounded-push is required here before item-7 makes returns
    /// concurrent.
    spare_count: crate::sync::AtomicU32,
    /// Eviction configuration and state.
    evict: Box<Eviction>,
    /// Max segments in the admission pool (S3-FIFO only, 0 for other policies).
    admission_cap: u32,
    /// Current number of segments in the admission pool.
    admission_count: u32,
}

/// Result of draining a segment: `Freed` means it was returned to the
/// free queue; `Deferred` means it was condemned to AwaitingRelease and
/// the last reader's guard drop will free it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearOutcome {
    Freed,
    Deferred,
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
            evict: Box::new(Eviction::new(segments, evict_policy)),
            admission_cap,
            admission_count: 0,
        })
    }

    // ── Pool helpers ─────────────────────────────────────────────────

    /// Check if the given pool has room for another segment.
    pub(crate) fn pool_has_room(&self, pool: SegmentPool) -> bool {
        match pool {
            SegmentPool::Admission => self.admission_count < self.admission_cap,
            SegmentPool::Main => true,
        }
    }

    /// Track a segment transitioning to the given pool.
    pub(crate) fn incr_pool(&mut self, pool: SegmentPool) {
        if pool == SegmentPool::Admission {
            self.admission_count += 1;
        }
    }

    // ── Accessors ────────────────────────────────────────────────────

    /// Return the configured eviction policy.
    #[inline]
    pub fn evict_policy(&self) -> Policy {
        self.evict.policy()
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

        if !header.try_acquire_reader() {
            return None;
        }
        // SAFETY: the acquire above succeeded, and both `headers` (a
        // boxed slice owned by `self`) and the boxed Injector outlive
        // any guard reachable through the public API.
        let guard = unsafe { SegmentGuard::new(header, &*self.free_queue) };

        let byte_offset = self.segment_size() as usize * (seg_id.get() as usize - 1) + offset;
        let raw = RawItem::from_ptr(unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) });
        Some((raw, guard))
    }

    /// Atomically reserve space for an item in the given segment,
    /// returning a `ReservedItem` for the granted region. `None` means
    /// the segment is full — the caller should expand the chain.
    ///
    /// Takes `&self`: the reservation is a header CAS and the item
    /// pointer is derived from the data base pointer, the same pattern
    /// as `get_item_at`.
    ///
    /// The `integrity` magic-byte check is intentionally skipped here
    /// (hot path); the debug-feature `check_integrity` scan covers it,
    /// the same idiom as `expiry_info`.
    ///
    /// SCOPE(writer-vs-drain): the reserve→define→publish window is not
    /// yet protected against a concurrent drain of this segment. Safe
    /// today because eviction and writers are serialized by `&mut
    /// Segcache`. Drain-safe merge (item 5b) made eviction itself
    /// reader-safe (no more in-place compaction of readable segments) but
    /// does not close this writer-vs-drain hazard; that protocol is
    /// deferred past 5b to item 7.
    pub(crate) fn try_alloc_item(&self, seg_id: NonZeroU32, size: i32) -> Option<ReservedItem> {
        debug_assert!(seg_id.get() <= self.cap);
        let header = self.header(seg_id);
        let offset = header.try_reserve_space(size, self.segment_size)?;

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
        Some(ReservedItem::new(
            RawItem::from_ptr(ptr),
            seg_id,
            offset as usize,
        ))
    }

    // ── Segment views ────────────────────────────────────────────────

    /// Returns a `Segment` view for the segment with the specified id. The
    /// header is borrowed as a shared reference (all fields are atomic) while
    /// the data slice is borrowed mutably.
    pub(crate) fn get_mut(&mut self, id: NonZeroU32) -> Result<Segment<'_>, SegmentsError> {
        let idx = id.get() as usize - 1;
        if idx < self.headers.len() {
            let header = &self.headers[idx];

            let seg_start = self.segment_size as usize * idx;
            let seg_end = self.segment_size as usize * (idx + 1);

            let seg_data = &mut self.data[seg_start..seg_end];

            let segment = Segment::from_raw_parts(header, seg_data);
            segment.check_magic();
            Ok(segment)
        } else {
            Err(SegmentsError::BadSegmentId)
        }
    }

    /// Gets a `Segment` view for two segments after ensuring the data borrows
    /// are disjoint. Because headers are shared refs (all fields are atomic),
    /// they can alias freely — we only need to split the data slice.
    pub(crate) fn get_mut_pair(
        &mut self,
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

        // Split data into non-overlapping slices.
        let seg_size = self.segment_size() as usize;

        // SAFETY: a_idx != b_idx is guaranteed above, so the data ranges are
        // disjoint. We split the mmap slice at the boundary between the two
        // lower-indexed and higher-indexed segments.
        {
            let data: &mut [u8] = &mut self.data;
            let split = (std::cmp::min(a_idx, b_idx) + 1) * seg_size;
            let (first, second) = data.split_at_mut(split);

            let (data_a, data_b) = if a_idx < b_idx {
                let start_a = seg_size * a_idx;
                let end_a = seg_size * (a_idx + 1);

                let start_b = (seg_size * b_idx) - first.len();
                let end_b = (seg_size * (b_idx + 1)) - first.len();

                (&mut first[start_a..end_a], &mut second[start_b..end_b])
            } else {
                let start_a = (seg_size * a_idx) - first.len();
                let end_a = (seg_size * (a_idx + 1)) - first.len();

                let start_b = seg_size * b_idx;
                let end_b = seg_size * (b_idx + 1);

                (&mut second[start_a..end_a], &mut first[start_b..end_b])
            };

            let segment_a = Segment::from_raw_parts(header_a, data_a);
            let segment_b = Segment::from_raw_parts(header_b, data_b);

            segment_a.check_magic();
            segment_b.check_magic();
            Ok((segment_a, segment_b))
        }
    }

    // ── Chain helpers ────────────────────────────────────────────────

    /// Unlink a segment from its chain by patching the prev/next pointers of
    /// its neighbours.
    ///
    /// *NOTE*: this must not be used on segments in the free queue.
    fn unlink(&mut self, id: NonZeroU32) {
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

    /// Link a Reserved segment at the front of a chain and publish it as
    /// Sealed (readable + evictable, never the write tail): Reserved ->
    /// Linking carries the next pointer, the old head's prev is patched,
    /// then Linking -> Sealed publishes.
    fn link_at_head(&mut self, this: NonZeroU32, head: Option<NonZeroU32>) {
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

        let sealed = self.headers[this_idx].cas_metadata(
            State::Linking,
            State::Sealed,
            None,
            None,
            Ordering::AcqRel,
        );
        debug_assert!(sealed, "linking segment must publish as Sealed");
    }

    // ── Free queue ───────────────────────────────────────────────────

    /// Return a drained segment to the free queue. The segment must be in
    /// the Draining state with no readers pinning it; its write statistics
    /// are reset (and its generation bumped) at reserve time.
    pub(crate) fn recycle(&mut self, id: NonZeroU32) {
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
            self.admission_count = self.admission_count.saturating_sub(1);
        }
        self.headers[id_idx].set_pool(SegmentPool::Main);

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
    fn return_segment(&self, id: u32) {
        // Check-then-act on `spare_count`: race-free only because eviction
        // is `&mut`-serialized (see the field doc). Needs a CAS/bounded-
        // push before this can be called concurrently (item-7).
        if self.spare_count.load(Ordering::Relaxed) < self.spare_capacity {
            self.spare_count.fetch_add(1, Ordering::Relaxed);
            self.spare_queue.push(id);
        } else {
            self.free_queue.push(id);
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
        &mut self,
        id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
        expire: bool,
    ) -> Result<ClearOutcome, ()> {
        let id_idx = id.get() as usize - 1;

        // SeqCst: writer half of the Dekker pair with try_acquire_reader
        // (transition the state, then observe the reader count).
        if !self.headers[id_idx].cas_metadata(
            State::Sealed,
            State::Draining,
            None,
            None,
            Ordering::SeqCst,
        ) {
            return Err(());
        }

        // Capture the links before condemn clears them.
        let meta = self.headers[id_idx].metadata(Ordering::Acquire);
        let (next, prev) = (meta.next, meta.prev);

        {
            let mut segment = self.get_mut(id).unwrap();
            segment.clear(hashtable, expire);
        }

        if self.headers[id_idx].ref_count_seqcst() == 0 {
            self.recycle(id);
            Ok(ClearOutcome::Freed)
        } else {
            Ok(self.condemn(id, next, prev))
        }
    }

    /// Condemn a drained, pinned segment: transition it to
    /// AwaitingRelease (chain-free) and hand reclamation to the last
    /// reader's guard drop. The hashtable must already be fully drained —
    /// that is what guarantees no NEW reader can pin an AwaitingRelease
    /// segment (no hashtable location routes to it), even though the
    /// state remains readable for in-flight pins.
    ///
    /// Returns `Freed` if the race-fix recheck discovered the last
    /// reader already dropped (this caller then reclaimed the segment),
    /// `Deferred` otherwise.
    pub(crate) fn condemn(
        &mut self,
        id: NonZeroU32,
        next: Option<NonZeroU32>,
        prev: Option<NonZeroU32>,
    ) -> ClearOutcome {
        let id_idx = id.get() as usize - 1;

        // Pool membership ends at condemn time (the guard drop has no
        // access to the pool bookkeeping).
        if self.headers[id_idx].pool() == SegmentPool::Admission {
            self.admission_count = self.admission_count.saturating_sub(1);
        }
        self.headers[id_idx].set_pool(SegmentPool::Main);

        let condemned = self.headers[id_idx].cas_metadata(
            State::Draining,
            State::AwaitingRelease,
            Some(None),
            Some(None),
            Ordering::SeqCst,
        );
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
        &mut self,
        ttl_buckets: &mut TtlBuckets,
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

        match self.evict.policy() {
            Policy::Merge { .. } => {
                #[cfg(feature = "metrics")]
                SEGMENT_EVICT.increment();

                let mut seg_idx = self.evict.random();

                seg_idx %= self.cap;
                let ttl = self.headers[seg_idx as usize].ttl();
                let offset = ttl_buckets.get_bucket_index(ttl);
                let buckets = ttl_buckets.buckets.len();

                // Since merging starts in the middle of a segment chain, we
                // may need to loop back around to the first TTL bucket.
                for i in 0..=buckets {
                    let bucket_id = (offset + i) % buckets;
                    let ttl_bucket = &mut ttl_buckets.buckets[bucket_id];
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
                    // Capture the links before the drain (condemn clears
                    // them) for the bucket head fixup below.
                    let id_idx = id.get() as usize - 1;
                    let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);

                    let outcome = self.clear_segment(id, hashtable, false);

                    #[cfg(feature = "metrics")]
                    EVICT_TIME.add(now.elapsed().as_nanos() as _);

                    match outcome {
                        Err(()) => Err(SegmentsError::EvictFailure),
                        Ok(outcome) => {
                            // The segment left its chain either way.
                            if meta.prev.is_none() {
                                let ttl_bucket =
                                    ttl_buckets.get_mut_bucket(self.headers[id_idx].ttl());
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
    pub(crate) fn least_valuable_seg(
        &mut self,
        ttl_buckets: &mut TtlBuckets,
    ) -> Option<NonZeroU32> {
        match self.evict.policy() {
            Policy::None => None,
            Policy::Random => {
                let mut start: u32 = self.evict.random();

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
                let mut start: u32 = self.evict.random();

                start %= self.cap;

                for i in 0..self.cap {
                    let idx = (start + i) % self.cap;
                    if self.headers[idx as usize].state().is_readable() {
                        let ttl = self.headers[idx as usize].ttl();
                        let ttl_bucket = ttl_buckets.get_mut_bucket(ttl);
                        return ttl_bucket.head();
                    }
                }

                None
            }
            _ => {
                if self.evict.should_rerank() {
                    self.evict.rerank(&self.headers);
                }
                while let Some(id) = self.evict.least_valuable_seg() {
                    if let Ok(seg) = self.get_mut(id) {
                        if seg.can_evict() {
                            return Some(id);
                        }
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
    pub(crate) fn remove_at(
        &mut self,
        seg_id: NonZeroU32,
        offset: usize,
        ttl_buckets: &mut TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Remove the item.
        {
            let segment = self.get_mut(seg_id)?;
            segment.remove_item_at(offset);

            // If the segment is now empty and evictable, free it
            // immediately via the drain/condemn protocol.
            if segment.live_items() == 0 && segment.can_evict() {
                let meta = segment.header_metadata();

                if self.clear_segment(seg_id, hashtable, false).is_ok() && meta.prev.is_none() {
                    let id_idx = seg_id.get() as usize - 1;
                    let ttl_bucket = ttl_buckets.get_mut_bucket(self.headers[id_idx].ttl());
                    ttl_bucket.set_head(meta.next);
                }
                return Ok(());
            }
        }

        // For merge eviction, check if the segment is below the compact ratio
        // low watermark. If so, perform a no-evict merge (compaction only).
        if let Policy::Merge { .. } = self.evict.policy() {
            let target_ratio = self.evict.compact_ratio();

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
                    let ttl_bucket = ttl_buckets.get_mut_bucket(ttl);
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
    fn merge_evict_chain_len(&mut self, start: NonZeroU32) -> usize {
        let mut len = 0;
        let mut id = start;
        let max = self.evict.max_merge();

        while len < max {
            if let Ok(seg) = self.get_mut(id) {
                if seg.can_evict() {
                    len += 1;
                    match seg.next_seg() {
                        Some(i) => {
                            id = i;
                        }
                        None => {
                            break;
                        }
                    }
                } else {
                    break;
                }
            } else {
                warn!("invalid segment id: {id}");
                break;
            }
        }

        len
    }

    /// Count how many evictable segments follow `start` whose combined live
    /// bytes fit within a single segment.
    fn merge_compact_chain_len(&mut self, start: NonZeroU32) -> usize {
        let mut len = 0;
        let mut id = start;
        let max = self.evict.max_merge();
        let mut occupied = 0;
        let seg_size = self.segment_size();

        while len < max {
            if let Ok(seg) = self.get_mut(id) {
                if seg.can_evict() {
                    occupied += seg.live_bytes();
                    if occupied > seg_size {
                        break;
                    }
                    len += 1;
                    match seg.next_seg() {
                        Some(i) => {
                            id = i;
                        }
                        None => {
                            break;
                        }
                    }
                } else {
                    break;
                }
            } else {
                warn!("invalid segment id: {id}");
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
        &mut self,
        start: NonZeroU32,
        ttl_bucket: &mut TtlBucket,
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

        // Configure the spare and head-insert it as Sealed exactly ONCE:
        // readable and evictable immediately, but never the write tail (the
        // tail is Live). Because the spare is never drained, the bucket head
        // points at it for the entire candidate loop below — draining a
        // candidate only unlinks it from the middle of the chain (its
        // neighbours are patched by clear_segment's recycle/condemn), so no
        // per-candidate head fixup is required here.
        let src_ttl = self.headers[start.get() as usize - 1].ttl();
        {
            let sidx = spare_id.get() as usize - 1;
            self.headers[sidx].set_ttl(src_ttl);
            self.headers[sidx].set_pool(SegmentPool::Main);
            self.headers[sidx].mark_merged();
        }
        let old_head = ttl_bucket.head();
        self.link_at_head(spare_id, old_head);
        ttl_bucket.set_head(Some(spare_id));

        // Merge state.
        let mut cutoff = 1.0;
        let mut merged = 0;

        // Fixed merge parameters.
        let max_merge = self.evict.max_merge();
        let n_merge = self.evict.n_merge();
        let stop_ratio = self.evict.stop_ratio();
        let stop_bytes = (stop_ratio * self.segment_size() as f64) as i32;

        // Dynamically set target ratio based on chain length.
        let target_ratio = if chain_len < n_merge {
            1.0 / chain_len as f64
        } else {
            self.evict.target_ratio()
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

            if !self
                .get_mut(cand_id)
                .map(|s| s.can_evict())
                .unwrap_or(false)
            {
                trace!("stop merge: can't evict candidate segment");
                break;
            }

            // Advance the chain pointer BEFORE draining the candidate: once
            // clear_segment recycles it, its links are reset and reading
            // next_seg() would observe a stale/reused segment.
            next_id = self.headers[cand_id.get() as usize - 1].next_seg();

            // Prune low-frequency items (marks them deleted — moves no
            // bytes), then copy the survivors into the spare. copy_into
            // appends past the spare's write-offset and republishes each
            // survivor via the hashtable's Release-CAS, so no readable
            // segment's live bytes are ever moved in place.
            {
                let mut cand = self.get_mut(cand_id)?;
                let cand_old_size = cand.live_bytes();
                cutoff = cand.prune(hashtable, cutoff, target_ratio);
                trace!(
                    "cand {cand_id}: {cand_old_size} bytes -> {} bytes after prune",
                    cand.live_bytes()
                );
            }
            {
                let (mut cand, mut spare) = self.get_mut_pair(cand_id, spare_id)?;
                let _ = cand.copy_into(&mut spare, hashtable);
            }

            // Drain the candidate (Sealed->Draining + ref_count recheck +
            // condemn-if-pinned). An unpinned candidate is recycled — which
            // replenishes the spare via return_segment; a pinned candidate is
            // condemned to its last reader. Either way it leaves the chain,
            // and clear_segment's unlink patches the neighbours, so the spare
            // remains the bucket head.
            match self.clear_segment(cand_id, hashtable, false) {
                Ok(_outcome) => {}
                Err(()) => break,
            }
            merged += 1;
        }

        Ok(next_id)
    }

    /// Graceful degradation when no spare is available: drop the chain head
    /// whole via the drain machinery, freeing one segment (which also
    /// replenishes the spare via `return_segment` on the next unpinned
    /// recycle). No spare was head-inserted, so this path DOES fix the bucket
    /// head when the dropped segment was itself the head.
    fn merge_evict_fallback_drop(
        &mut self,
        start: NonZeroU32,
        ttl_bucket: &mut TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
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
        &mut self,
        start: NonZeroU32,
        ttl_bucket: &mut TtlBucket,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<Option<NonZeroU32>, SegmentsError> {
        #[cfg(feature = "metrics")]
        SEGMENT_MERGE.increment();

        let chain_len = self.merge_compact_chain_len(start);

        if chain_len < 2 {
            return Err(SegmentsError::NoEvictableSegments);
        }

        let next_id = self.get_mut(start).map(|s| s.next_seg())?;

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

        // Configure the spare and head-insert it as Sealed exactly ONCE:
        // same chain-head invariant as merge_evict (see its comment) — the
        // spare is never drained, so no per-candidate head fixup is needed.
        let src_ttl = self.headers[start.get() as usize - 1].ttl();
        {
            let sidx = spare_id.get() as usize - 1;
            self.headers[sidx].set_ttl(src_ttl);
            self.headers[sidx].set_pool(SegmentPool::Main);
            self.headers[sidx].mark_merged();
        }
        let old_head = ttl_bucket.head();
        self.link_at_head(spare_id, old_head);
        ttl_bucket.set_head(Some(spare_id));

        // Merge state.
        let mut merged = 0;

        // Fixed merge parameters.
        let seg_size = self.segment_size();
        let max_merge = self.evict.max_merge();
        let stop_ratio = self.evict.stop_ratio();
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

            if !self
                .get_mut(cand_id)
                .map(|s| s.can_evict())
                .unwrap_or(false)
            {
                trace!("stop merge: can't evict candidate segment");
                break;
            }

            let spare_size = self.headers[spare_id.get() as usize - 1].live_bytes();
            let cand_size = self.headers[cand_id.get() as usize - 1].live_bytes();

            if spare_size >= stop_bytes || spare_size + cand_size > seg_size {
                trace!("stop merge: spare segment is full");
                break;
            }

            // Advance the chain pointer BEFORE draining the candidate: once
            // clear_segment recycles it, its links are reset and reading
            // next_seg() would observe a stale/reused segment.
            next_id = self.headers[cand_id.get() as usize - 1].next_seg();

            // Copy the survivors into the spare. copy_into appends past the
            // spare's write-offset and republishes each survivor via the
            // hashtable's Release-CAS, so no readable segment's live bytes
            // are ever moved in place.
            {
                let (mut cand, mut spare) = self.get_mut_pair(cand_id, spare_id)?;
                let _ = cand.copy_into(&mut spare, hashtable);
            }

            // Drain the candidate (Sealed->Draining + ref_count recheck +
            // condemn-if-pinned). An unpinned candidate is recycled — which
            // replenishes the spare via return_segment; a pinned candidate is
            // condemned to its last reader. Either way it leaves the chain,
            // and clear_segment's unlink patches the neighbours, so the spare
            // remains the bucket head.
            match self.clear_segment(cand_id, hashtable, false) {
                Ok(_outcome) => {}
                Err(()) => break,
            }
            merged += 1;
        }

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
        &mut self,
        ttl_buckets: &mut TtlBuckets,
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
        &mut self,
        seg_id: NonZeroU32,
        ttl_buckets: &mut TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // First pass: copy items with freq > 0 into a main-pool segment.
        let target_id = self.reserve_free();

        if let Some(tid) = target_id {
            self.headers[tid.get() as usize - 1].set_pool(SegmentPool::Main);

            let src_ttl = self.headers[seg_id.get() as usize - 1].ttl();
            self.headers[tid.get() as usize - 1].set_ttl(src_ttl);
            // Link the target at the head of the TTL bucket, then publish
            // it as Sealed: the target is readable and evictable
            // immediately, but is never the write tail, so it must not
            // pass through Live (Live == the bucket tail reserve()
            // writes into).
            let ttl_bucket = ttl_buckets.get_mut_bucket(src_ttl);
            let old_head = ttl_bucket.head();
            self.link_at_head(tid, old_head);
            ttl_bucket.set_head(Some(tid));

            self.s3fifo_promote_from(seg_id, tid, hashtable);
        }
        // If no free segment, we just drop everything (all items evicted).

        // Add hashes of remaining (freq == 0) items to ghost queue.
        self.s3fifo_ghost_remaining(seg_id, hashtable);

        // Drain the source; clear_segment frees or condemns it.
        let id_idx = seg_id.get() as usize - 1;
        let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);
        let outcome = self
            .clear_segment(seg_id, hashtable, false)
            .map_err(|_| SegmentsError::EvictFailure)?;

        // The segment left its chain either way.
        if meta.prev.is_none() {
            let ttl_bucket = ttl_buckets.get_mut_bucket(self.headers[id_idx].ttl());
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
        &mut self,
        src_id: NonZeroU32,
        dst_id: NonZeroU32,
        hashtable: &MultiChoiceHashtable,
    ) {
        let seg_size = self.segment_size() as usize;
        let (src, dst) = match self.get_mut_pair(src_id, dst_id) {
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

            offset += item_size;
        }
    }

    /// Add hashes of remaining live items in a segment to the ghost queue.
    fn s3fifo_ghost_remaining(&mut self, seg_id: NonZeroU32, hashtable: &MultiChoiceHashtable) {
        // Collect hashes first to avoid borrow conflict with self.evict.ghost.
        let mut hashes = Vec::new();
        {
            let segment = match self.get_mut(seg_id) {
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

        for hash in hashes {
            self.evict.ghost.insert(hash);
        }
    }

    /// Evict a main-pool segment using CLOCK-style second chance. Items with
    /// freq > 0 are copied to a fresh main segment. Items with freq == 0 are
    /// dropped.
    fn s3fifo_evict_main(
        &mut self,
        seg_id: NonZeroU32,
        ttl_buckets: &mut TtlBuckets,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        // Try to get a target segment for second-chance items.
        let target_id = self.reserve_free();

        if let Some(tid) = target_id {
            self.headers[tid.get() as usize - 1].set_pool(SegmentPool::Main);

            let src_ttl = self.headers[seg_id.get() as usize - 1].ttl();
            self.headers[tid.get() as usize - 1].set_ttl(src_ttl);
            // Head insert + publish as Sealed (see s3fifo_evict_admission).
            let ttl_bucket = ttl_buckets.get_mut_bucket(src_ttl);
            let old_head = ttl_bucket.head();
            self.link_at_head(tid, old_head);
            ttl_bucket.set_head(Some(tid));

            // Copy freq > 0 items (same promote logic, but no ghost).
            self.s3fifo_promote_from(seg_id, tid, hashtable);
        }

        // Drain the source; clear_segment frees or condemns it.
        let id_idx = seg_id.get() as usize - 1;
        let meta = self.headers[id_idx].metadata(crate::sync::Ordering::Acquire);
        let outcome = self
            .clear_segment(seg_id, hashtable, false)
            .map_err(|_| SegmentsError::EvictFailure)?;

        // The segment left its chain either way.
        if meta.prev.is_none() {
            let ttl_bucket = ttl_buckets.get_mut_bucket(self.headers[id_idx].ttl());
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

    /// Check if a key hash is in the ghost queue (S3-FIFO).
    pub(crate) fn ghost_contains(&self, hash: u64) -> bool {
        self.evict.ghost.contains(hash)
    }

    /// Remove a hash from the ghost queue (on ghost hit).
    pub(crate) fn ghost_remove(&mut self, hash: u64) {
        self.evict.ghost.remove(hash);
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
        use crate::sync::Ordering;
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

        let mut cache = Segcache::builder()
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
            cache.ttl_buckets.get_mut_bucket(seg_ttl).head(),
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
            let bucket = cache.ttl_buckets.get_mut_bucket(seg_ttl);
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
            cache.ttl_buckets.get_mut_bucket(seg_ttl).head(),
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
}
