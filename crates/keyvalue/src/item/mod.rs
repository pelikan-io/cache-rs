//! Items are the base unit of data stored within a cache.

mod header;
mod raw;

#[cfg(any(feature = "integrity", feature = "debug"))]
pub use header::BASIC_INTEGRITY_SIZE;

pub use header::{BasicHeader, BASIC_HDR_SIZE};
pub use raw::RawItem;
