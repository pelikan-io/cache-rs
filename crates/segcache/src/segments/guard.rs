//! RAII pin on a segment's reader count.

use crate::segments::SegmentHeader;

/// An RAII guard representing one reader pin on a segment.
///
/// While a `SegmentGuard` is alive, the pinned segment cannot be
/// recycled, merged, or compacted — eviction, expiration, and clear all
/// skip segments with a non-zero reader count. The guard is released on
/// drop.
///
/// Holds a raw pointer rather than a borrow so that the guard (and the
/// [`crate::Item`] carrying it) is not lifetime-tied to the cache; this
/// is the same contract `RawItem` already has with the segment data.
pub(crate) struct SegmentGuard {
    header: *const SegmentHeader,
}

impl SegmentGuard {
    /// Create a guard for a successfully acquired reader pin.
    ///
    /// # Safety
    ///
    /// - `SegmentHeader::try_acquire_reader` must have returned `true`
    ///   on `header`, and ownership of that pin transfers to this guard.
    /// - `header` must point into the `Segments` headers allocation,
    ///   which must outlive the guard.
    pub(crate) unsafe fn new(header: *const SegmentHeader) -> Self {
        Self { header }
    }
}

impl Drop for SegmentGuard {
    fn drop(&mut self) {
        // SAFETY: per the constructor contract, the header outlives the
        // guard and holds a pin we own.
        unsafe { (*self.header).release_reader() }
    }
}
