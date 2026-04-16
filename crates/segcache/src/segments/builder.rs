//! Builder for configuring segment storage.

use crate::eviction::*;
use crate::segments::*;
use crate::*;

/// Configuration builder for [`Segments`].
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
    ///
    /// # Panics
    ///
    /// Panics if the size is not greater than the per-item overhead.
    pub fn segment_size(mut self, bytes: i32) -> Self {
        #[cfg(not(feature = "magic"))]
        assert!(bytes > ITEM_HDR_SIZE as i32);

        #[cfg(feature = "magic")]
        assert!(bytes > ITEM_HDR_SIZE as i32 + ITEM_MAGIC_SIZE as i32);

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

    /// Build the [`Segments`] from this configuration.
    pub fn build(self) -> Result<Segments, std::io::Error> {
        Segments::from_builder(self)
    }
}
