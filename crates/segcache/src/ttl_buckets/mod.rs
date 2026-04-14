// Copyright 2021 Twitter, Inc.
// Copyright 2026 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! TTL buckets are used to group items by TTL to enable eager expiration.
//!
//! The total collection of [`TtlBuckets`] is a contiguous allocation of
//! [`TtlBucket`]s which cover the full range of TTLs.
//!
//! Each [`TtlBucket`] contains a segment chain holding items with a similar
//! TTL. See the blog post for more details on the segcache design:
//! <https://twitter.github.io/pelikan/2021/segcache.html>
//!

mod error;
mod ttl_bucket;
#[allow(clippy::module_inception)]
mod ttl_buckets;

#[cfg(test)]
mod tests;

pub use error::TtlBucketsError;
pub use ttl_bucket::TtlBucket;
pub use ttl_buckets::TtlBuckets;
