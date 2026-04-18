//! Items are the base unit of data stored within a cache.
//!
//! An item consists of a packed header followed by optional data, key bytes,
//! and value bytes. The [`RawItem`] type provides byte-level access to this
//! representation through a raw pointer.

mod header;
mod raw;

#[cfg(any(feature = "integrity", feature = "debug"))]
pub use header::ITEM_INTEGRITY_SIZE;

pub use header::{ItemHeader, ITEM_HDR_SIZE};
pub use raw::RawItem;
