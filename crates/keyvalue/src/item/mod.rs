//! Items are the base unit of data stored within a cache.
//!
//! An item consists of a packed header followed by optional data, key bytes,
//! and value bytes. The [`RawItem`] type provides byte-level access to this
//! representation through a raw pointer.

mod header;
mod raw;

use crate::Value;

#[cfg(any(feature = "integrity", feature = "debug"))]
pub use header::ITEM_INTEGRITY_SIZE;

pub use header::{ItemHeader, ITEM_HDR_SIZE};
pub use raw::RawItem;

/// Trait for zero-copy read access to a cache item's data.
///
/// Implemented by types returned from cache lookup operations.
/// The `'a` lifetime ties the returned slices to the underlying storage.
/// The `Send` bound prepares the interface for concurrent access when
/// ref-counted segment guards are introduced.
pub trait ItemGuard<'a>: Send {
    fn key(&self) -> &[u8];
    fn value(&self) -> Value<'_>;
    fn optional(&self) -> &[u8];
}
