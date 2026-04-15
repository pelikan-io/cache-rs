// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Cuckoo hash based cache storage engine with fixed-size item slots.
//!
//! This crate implements an array-based cache that uses cuckoo hashing to map
//! keys to one of D=4 candidate positions in a flat array of fixed-size slots.
//! When all candidate positions for a new key are occupied, existing items may
//! be displaced to their alternative positions, up to a configurable depth.
//! If displacement fails, an item is evicted according to the configured policy.
//!
//! The design is based on the cuckoo storage engine from Pelikan:
//! <https://github.com/pelikan-io/pelikan>
//!
//! Goals:
//! * O(1) lookup with bounded worst case
//! * fixed per-item memory overhead
//! * bounded insertion latency via displacement limits
//!
//! Non-goals:
//! * not designed for concurrent access
//! * not suited for items larger than the configured slot size

#[macro_use]
extern crate log;

mod builder;
mod cuckoo;
mod error;
mod item;

#[cfg(feature = "metrics")]
mod metrics;

#[cfg(test)]
mod tests;

pub use builder::Builder;
pub use cuckoo::CuckooCache;
pub use error::CuckooCacheError;
pub use item::Item;
pub use keyvalue::Value;

/// Eviction policy used when all candidate positions are occupied and
/// displacement cannot free a slot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Select a random candidate for eviction.
    #[default]
    Random,
    /// Prefer evicting the candidate with the nearest expiration time.
    Expire,
}

/// The number of candidate hash positions per item (D in the cuckoo
/// hashing literature).
pub(crate) const D: usize = 4;
