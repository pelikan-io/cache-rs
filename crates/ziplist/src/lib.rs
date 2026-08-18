#![no_std]
#![forbid(unsafe_code)]

//! Compact byte-format collections codec for cache storage.
//!
//! This crate provides encoding and decoding facilities for ziplist blocks,
//! a compact binary format for storing collections (lists, hashes, sets,
//! sorted sets) with minimal memory overhead.

pub mod block;
pub mod cursor;
pub mod entry;
pub mod error;
pub mod header;

pub use block::{Block, BlockMut, InsertPos};
pub use cursor::{locate, Cursor};
pub use entry::{
    canonical_uint, compare, compare_raw, decode, decode_backward, encode_into, encoded_len,
    render_uint, EntryVal,
};
pub use error::{DecodeError, Fit, NeedBytes};
pub use header::{BlockHeader, Type, FLAG_CHAIN_ROOT, HEADER_SIZE};
