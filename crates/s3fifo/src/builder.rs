//! A builder for configuring a new [`S3Fifo`] instance.

use crate::*;
use std::collections::VecDeque;

const DEFAULT_HEAP_SIZE: usize = 64 * 1024 * 1024; // 64 MB
const DEFAULT_SMALL_QUEUE_RATIO: f64 = 0.10;

/// A builder that is used to construct a new [`S3Fifo`] instance.
pub struct Builder {
    hash_power: u8,
    overflow_factor: f64,
    heap_size: usize,
    small_queue_ratio: f64,
    ghost_capacity: Option<usize>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            hash_power: 16,
            overflow_factor: 0.0,
            heap_size: DEFAULT_HEAP_SIZE,
            small_queue_ratio: DEFAULT_SMALL_QUEUE_RATIO,
            ghost_capacity: None,
        }
    }
}

impl Builder {
    /// Specify the hash power, which limits the size of the hashtable to 2^N
    /// entries. The total number of items which can be held is limited to
    /// `7 * 2^(N - 3)` items without overflow buckets.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// let cache = S3Fifo::builder().hash_power(17).build();
    /// let cache = S3Fifo::builder().hash_power(21).build();
    /// ```
    pub fn hash_power(mut self, hash_power: u8) -> Self {
        assert!(hash_power >= 3, "hash power must be at least 3");
        self.hash_power = hash_power;
        self
    }

    /// Specify an overflow factor which is used to scale the hashtable and
    /// provide additional capacity for chaining item buckets.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// let cache = S3Fifo::builder()
    ///     .hash_power(17)
    ///     .overflow_factor(1.0)
    ///     .build();
    /// ```
    pub fn overflow_factor(mut self, percent: f64) -> Self {
        self.overflow_factor = percent;
        self
    }

    /// Specify the total number of bytes to be used for item storage. This
    /// is the logical capacity against which items are tracked.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// let cache = S3Fifo::builder().heap_size(64 * MB).build();
    /// let cache = S3Fifo::builder().heap_size(256 * MB).build();
    /// ```
    pub fn heap_size(mut self, bytes: usize) -> Self {
        self.heap_size = bytes;
        self
    }

    /// Specify the ratio of the cache dedicated to the small FIFO queue.
    /// Defaults to 0.10 (10%). The remaining capacity is used for the main
    /// queue.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// let cache = S3Fifo::builder()
    ///     .small_queue_ratio(0.10)
    ///     .build();
    /// ```
    pub fn small_queue_ratio(mut self, ratio: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&ratio),
            "small queue ratio must be between 0.0 and 1.0"
        );
        self.small_queue_ratio = ratio;
        self
    }

    /// Specify the capacity of the ghost queue (number of fingerprints to
    /// retain). If not set, this is automatically sized based on the heap
    /// size and small queue ratio.
    pub fn ghost_capacity(mut self, capacity: usize) -> Self {
        self.ghost_capacity = Some(capacity);
        self
    }

    /// Consumes the builder and returns a fully-allocated `S3Fifo` instance.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// let cache = S3Fifo::builder()
    ///     .heap_size(64 * MB)
    ///     .hash_power(16)
    ///     .build();
    /// ```
    pub fn build(self) -> Result<S3Fifo, std::io::Error> {
        let hashtable = HashTable::new(self.hash_power, self.overflow_factor);
        let small_quota = (self.heap_size as f64 * self.small_queue_ratio) as usize;

        // Auto-size ghost queue: approximate the number of items that could
        // fit in the small queue (assuming ~64 byte average items)
        let ghost_capacity = self
            .ghost_capacity
            .unwrap_or_else(|| std::cmp::max(1024, small_quota / 64));

        Ok(S3Fifo {
            hashtable,
            slab: Slab::new(),
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: GhostQueue::new(ghost_capacity),
            heap_size: self.heap_size,
            small_quota,
            current_bytes: 0,
            small_bytes: 0,
            live_items: 0,
            started: Instant::now(),
        })
    }
}
