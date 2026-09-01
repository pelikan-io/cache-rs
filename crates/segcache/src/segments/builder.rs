//! Builder for configuring segment storage.

use crate::eviction::*;
use crate::segments::*;
use crate::Location;

/// Configuration builder for [`Segments`].
///
/// Validation is deferred to [`build()`](SegmentsBuilder::build) so that
/// setters never panic.
pub(crate) struct SegmentsBuilder {
    pub(super) heap_size: usize,
    pub(super) segment_size: i32,
    pub(super) evict_policy: Policy,
}

impl Default for SegmentsBuilder {
    fn default() -> Self {
        Self {
            segment_size: 1024 * 1024,
            heap_size: 64 * 1024 * 1024,
            evict_policy: Policy::Random,
        }
    }
}

impl SegmentsBuilder {
    /// Set the segment size in bytes.
    pub fn segment_size(mut self, bytes: i32) -> Self {
        self.segment_size = bytes;
        self
    }

    /// Set the total heap size in bytes. The number of segments is
    /// `heap_size / segment_size`.
    pub fn heap_size(mut self, bytes: usize) -> Self {
        self.heap_size = bytes;
        self
    }

    /// Set the eviction [`Policy`].
    pub fn eviction_policy(mut self, policy: Policy) -> Self {
        self.evict_policy = policy;
        self
    }

    /// Validate configuration and build the [`Segments`].
    ///
    /// Returns an error if:
    /// - `segment_size` is not larger than the per-item header overhead
    /// - `segment_size` exceeds `Location::MAX_SEGMENT_BYTES`, the most a
    ///   location's offset field can address (`SegmentsError::SegmentTooLarge`)
    /// - `heap_size` is zero or not a multiple of `segment_size`
    /// - the resulting segment count exceeds `Location::MAX_SEGMENTS`, the
    ///   largest id a heap may issue — one below what a location's 18-bit
    ///   segment field could address, the top value being reserved so no
    ///   location aliases the ghost sentinel
    ///   (`SegmentsError::TooManySegments`, raised by `Segments::from_builder`)
    pub fn build(self) -> Result<Segments, SegmentsError> {
        let min_size = crate::ITEM_HDR_SIZE as i32 + 1;

        if self.segment_size < min_size {
            return Err(SegmentsError::SegmentTooSmall);
        }

        // A location's offset field encodes `byte_offset >> 3` in
        // `OFFSET_BITS` bits behind only a debug_assert in pack_location; a
        // segment larger than it can address would wrap silently in release
        // builds, aliasing two live items onto one location.
        if self.segment_size as usize > Location::MAX_SEGMENT_BYTES {
            return Err(SegmentsError::SegmentTooLarge {
                segment_size: self.segment_size as usize,
                limit: Location::MAX_SEGMENT_BYTES,
            });
        }

        // Items are placed at 8-byte-aligned offsets and locations encode
        // `offset >> 3` (see pack_location), so segment bases must be
        // 8-aligned for absolute item addresses to be.
        if self.segment_size % 8 != 0 {
            return Err(SegmentsError::SegmentSizeUnaligned);
        }

        let seg_size = self.segment_size as usize;
        if self.heap_size == 0 || !self.heap_size.is_multiple_of(seg_size) {
            return Err(SegmentsError::InvalidHeapSize {
                heap_size: self.heap_size,
                segment_size: seg_size,
            });
        }

        Segments::from_builder(self)
    }
}
