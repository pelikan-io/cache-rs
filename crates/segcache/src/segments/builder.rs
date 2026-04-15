// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! A builder struct for initializing segment storage.

use crate::eviction::*;
use crate::segments::*;

/// The `SegmentsBuilder` allows for the configuration of the segment storage.
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
    /// This function will panic if the size is not greater than the per-item
    /// overhead. Currently this means that the minimum size is 6 bytes when
    /// built without magic/debug, or 10 bytes when built with magic/debug.
    pub fn segment_size(mut self, bytes: i32) -> Self {
        #[cfg(not(feature = "magic"))]
        assert!(bytes > ITEM_HDR_SIZE as i32);

        #[cfg(feature = "magic")]
        assert!(bytes > ITEM_HDR_SIZE as i32 + ITEM_MAGIC_SIZE as i32);

        self.segment_size = bytes;
        self
    }

    /// Specify the total heap size in bytes. The heap size will be divided by
    /// the segment size to determine the number of segments to allocate.
    pub fn heap_size(mut self, bytes: usize) -> Self {
        self.heap_size = bytes;
        self
    }

    /// Specify the eviction [`Policy`] which will be used when item allocation
    /// fails due to memory pressure.
    pub fn eviction_policy(mut self, policy: Policy) -> Self {
        self.evict_policy = policy;
        self
    }

    /// Construct the [`Segments`] from the builder
    pub fn build(self) -> Result<Segments, std::io::Error> {
        Segments::from_builder(self)
    }
}
