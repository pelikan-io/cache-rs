//! Shared key-value item types for cache storage engines.
//!
//! This crate provides the common item representation and value types used
//! across cache implementations such as segcache and s3fifo. Items are stored
//! as packed byte buffers with a compact header encoding key length, value
//! length, and optional metadata.

pub mod item;
pub mod tiny;
mod value;

pub use item::{ItemHeader, RawItem, ITEM_HDR_SIZE};
pub use tiny::{TinyItem, TinyItemHeader, TINY_ITEM_HDR_SIZE};
pub use value::{OwnedValue, Value};

#[cfg(any(feature = "magic", feature = "debug"))]
pub use item::ITEM_MAGIC_SIZE;

/// A simple error indicating the item value is not a numeric type.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct NotNumericError;

/// Returns the byte size of a value.
pub fn size_of(value: &Value) -> usize {
    match value {
        Value::Bytes(v) => v.len(),
        Value::U64(_) => core::mem::size_of::<u64>(),
    }
}
