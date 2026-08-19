#![no_std]
#![forbid(unsafe_code)]

//! Compact byte-format collections codec for cache storage.
//!
//! This crate provides encoding and decoding facilities for ziplist blocks,
//! a compact binary format for storing collections (lists, hashes, sets,
//! sorted sets) with minimal memory overhead.
//!
//! The normative format reference — block layout, entry tag tiers, backlen
//! encoding, per-type body conventions, and the format-evolution rule — is
//! `docs/ziplist.md` at the repository root; the byte layouts it freezes
//! are pinned by `tests/golden.rs`.

pub mod block;
pub mod cursor;
pub mod entry;
pub mod error;
pub mod hash;
pub mod header;
#[cfg(kani)]
mod kani;
pub mod list;
mod map;
pub mod set;
pub mod zset;

pub use block::{Block, BlockMut, InsertPos};
pub use cursor::{locate, Cursor};
pub use entry::{
    canonical_uint, compare, compare_raw, decode, decode_backward, encode_into, encoded_len,
    render_uint, EntryVal,
};
pub use error::{DecodeError, Fit, NeedBytes};
pub use hash::{HSet, HashMut, HashView, IncrError};
pub use header::{BlockHeader, Type, FLAG_CHAIN_ROOT, HEADER_SIZE};
pub use list::{ListMut, ListView};
pub use set::{SAdd, SetMut, SetView};
pub use zset::{Bound, ZAdd, ZsetMut, ZsetView};
