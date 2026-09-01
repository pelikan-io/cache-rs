//! RAII pin on a segment's reader count.

use crate::segments::SegmentHeader;

/// An RAII guard representing one reader pin on a segment.
///
/// While a `SegmentGuard` is alive, the pinned segment cannot be
/// recycled, merged, or compacted — eviction, expiration, and clear all
/// skip segments with a non-zero reader count, condemning them to
/// AwaitingRelease instead of freeing them.
///
/// The guard completes that handoff: when the LAST pin drops on a
/// condemned segment, the guard's drop transitions it AwaitingRelease ->
/// Free and returns it to the free queue directly — no `&mut Segments`
/// pass required. The transition CAS guarantees exactly-one-free among the
/// three claimants: a racing last-guard drop, the condemner's recheck, and
/// the backout of an acquire that failed after its increment.
///
/// Holds raw pointers rather than borrows so that the guard (and the
/// [`crate::Item`] carrying it) is not lifetime-tied to the cache; this
/// is the same contract `RawItem` already has with the segment data.
pub(crate) struct SegmentGuard {
    header: *const SegmentHeader,
    free_queue: *const crossbeam_deque::Injector<u32>,
}

impl SegmentGuard {
    /// Create a guard for a successfully acquired reader pin.
    ///
    /// # Safety
    ///
    /// - `SegmentHeader::try_acquire_reader` must have returned
    ///   `AcquireOutcome::Acquired` on `header`, and ownership of that pin
    ///   transfers to this guard.
    /// - `header` must point into the `Segments` headers allocation and
    ///   `free_queue` at the `Segments`-owned boxed Injector; both must
    ///   outlive the guard.
    pub(crate) unsafe fn new(
        header: *const SegmentHeader,
        free_queue: *const crossbeam_deque::Injector<u32>,
    ) -> Self {
        Self { header, free_queue }
    }
}

impl Drop for SegmentGuard {
    fn drop(&mut self) {
        // SAFETY: per the constructor contract, the header and the free
        // queue outlive the guard, and the guard owns one pin.
        let header = unsafe { &*self.header };

        // SeqCst decrement: the release-side Dekker pair. The condemner
        // CASes to AwaitingRelease (SeqCst) and then re-reads ref_count;
        // we decrement and then read the state. Weaker orderings permit
        // the interleaving where both sides see the other's old value —
        // the condemner sees a pin and defers, we see a not-yet-condemned
        // state and walk away, and the segment leaks.
        let prev = header.release_reader_for_guard();

        if prev == 1 && header.try_release_condemned() {
            // We were the last reader of a condemned segment and won the
            // AwaitingRelease -> Free transition: return it to the free
            // queue ourselves.
            //
            // Settle the segment's accounting FIRST. The CAS we just won is
            // exclusive (exactly one of the three claimants listed in the
            // struct doc above can win it) and nothing can find the segment
            // until we push it, so
            // we are its sole owner here — the same ownership `recycle` has
            // when it calls this. The other two claimants settle the same
            // way, at their own `return_segment`. Without it a condemned
            // segment carries its unpinned-unlink residue AND its whole dead
            // total onto the free queue, unreconciled until some later
            // `try_reserve` happens to pick it up: `item_current` would sit
            // above the true live count and `item_dead` above the true dead
            // occupancy for as long as the segment stays free (issue #58
            // part 2). `reset_write_stats` is idempotent, so the later
            // reserve-time reset is harmless.
            header.reset_write_stats();

            // The push below intentionally bypasses the spare-aware `return_segment`
            // helper: this guard only holds a raw pointer to the free
            // queue (see the struct doc), not `&Segments`, so it cannot
            // see or update the spare queue/count. A segment freed here
            // always lands in the free queue; the held-back spare
            // self-heals on the next unpinned `recycle`/`condemn` return.
            unsafe { (*self.free_queue).push(header.id().get()) };

            #[cfg(feature = "metrics")]
            {
                crate::SEGMENT_RETURN.increment();
                crate::SEGMENT_FREE.increment();
            }
        }
    }
}
