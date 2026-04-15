// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Builder for configuring a [`CuckooCache`] instance.

use crate::{CuckooCache, Policy};

/// A builder used to configure and construct a [`CuckooCache`].
pub struct Builder {
    pub(crate) item_size: usize,
    pub(crate) nitem: usize,
    pub(crate) max_displace: usize,
    pub(crate) policy: Policy,
    pub(crate) max_ttl: u32,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            item_size: 64,
            nitem: 1024,
            max_displace: 2,
            policy: Policy::Random,
            max_ttl: 2_592_000, // 30 days in seconds
        }
    }
}

impl Builder {
    /// Set the fixed size in bytes for each item slot. Key, value, and
    /// per-item overhead must fit within this size.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder().item_size(128).build();
    /// ```
    pub fn item_size(mut self, bytes: usize) -> Self {
        self.item_size = bytes;
        self
    }

    /// Set the maximum number of items the cache can hold.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder().nitem(4096).build();
    /// ```
    pub fn nitem(mut self, nitem: usize) -> Self {
        self.nitem = nitem;
        self
    }

    /// Set the maximum displacement depth during insertion. Higher values
    /// reduce evictions at the cost of longer worst-case insertion times.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder().max_displace(4).build();
    /// ```
    pub fn max_displace(mut self, depth: usize) -> Self {
        self.max_displace = depth;
        self
    }

    /// Set the eviction policy.
    ///
    /// ```
    /// use cuckoo_cache::{CuckooCache, Policy};
    ///
    /// let cache = CuckooCache::builder().policy(Policy::Expire).build();
    /// ```
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the maximum TTL in seconds. Items with a TTL exceeding this value
    /// will have their TTL clamped.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder().max_ttl(86400).build();
    /// ```
    pub fn max_ttl(mut self, seconds: u32) -> Self {
        self.max_ttl = seconds;
        self
    }

    /// Consume the builder and allocate a [`CuckooCache`].
    ///
    /// ```
    /// use cuckoo_cache::{CuckooCache, Policy};
    ///
    /// let cache = CuckooCache::builder()
    ///     .nitem(4096)
    ///     .item_size(64)
    ///     .policy(Policy::Random)
    ///     .build();
    /// ```
    pub fn build(self) -> CuckooCache {
        CuckooCache::from_builder(self)
    }
}
