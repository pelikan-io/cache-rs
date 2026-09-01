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

/// Alignment pad inserted between the key and the value slot of a
/// numeric item, bringing the value to an 8-byte boundary (relative to
/// the 8-aligned item start). Derived from stored header fields — never
/// persisted.
#[inline]
pub fn numeric_value_pad(klen: usize, olen: usize) -> usize {
    (8 - ((ITEM_HDR_SIZE + olen + klen) % 8)) % 8
}

/// Total item size for the given key/value/optional, matching
/// [`RawItem::size`]: numeric items include the alignment pad and the
/// seqlock version word. Reservation and the segment scan must agree on
/// this — use it everywhere an item's footprint is computed.
#[inline]
pub fn item_size(klen: usize, value: &crate::Value, olen: usize) -> usize {
    let extra = match value {
        crate::Value::U64(_) => numeric_value_pad(klen, olen) + 8,
        crate::Value::Bytes(_) => 0,
    };
    item_size_for(klen, olen, extra, crate::size_of(value))
}

/// The pure size arithmetic behind [`item_size`], separated so the Kani
/// harness proves the FUNCTION both value shapes call rather than a
/// transcription of its formula (the same structure-over-discipline move
/// as segcache's `bucket_index`).
#[inline]
fn item_size_for(klen: usize, olen: usize, extra: usize, vlen: usize) -> usize {
    let raw = ITEM_HDR_SIZE + olen + klen + extra + vlen;
    ((raw >> 3) + 1) << 3
}

pub use header::{ItemHeader, ITEM_HDR_SIZE};
pub use raw::NumericVersionGuard;
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

#[cfg(kani)]
mod verification {
    use super::*;

    /// The numeric alignment pad always lands the value slot on an
    /// 8-byte boundary (relative to the 8-aligned item start), and never
    /// wastes a full word.
    #[kani::proof]
    fn numeric_pad_aligns_value_slot() {
        let klen: usize = kani::any();
        kani::assume(klen <= 255);
        let olen: usize = kani::any();
        kani::assume(olen <= 63);
        let pad = numeric_value_pad(klen, olen);
        assert!(pad < 8);
        assert_eq!((ITEM_HDR_SIZE + olen + klen + pad) % 8, 0);
    }

    /// `item_size` covers the raw item bytes and is 8-byte aligned for
    /// every klen, olen, and value length — proven against the one pure
    /// function both value shapes call ([`item_size_for`]), plus the
    /// numeric shape end-to-end through the public `item_size`.
    /// Reservation and the segment scan both trust these properties.
    #[kani::proof]
    fn item_size_covers_and_aligns() {
        let klen: usize = kani::any();
        kani::assume(klen <= 255);
        let olen: usize = kani::any();
        kani::assume(olen <= 63);
        let vlen: u32 = kani::any();

        // Bytes shape: extra = 0, arbitrary vlen, through the real
        // arithmetic (the value slice's contents are irrelevant to size).
        let raw_bytes = ITEM_HDR_SIZE + olen + klen + vlen as usize;
        let size = item_size_for(klen, olen, 0, vlen as usize);
        assert!(size % 8 == 0 && size > raw_bytes && size - raw_bytes <= 8);

        // Numeric shape, end-to-end via the public function.
        let size = item_size(klen, &crate::Value::U64(0), olen);
        let raw = ITEM_HDR_SIZE + olen + klen + numeric_value_pad(klen, olen) + 16;
        assert!(size % 8 == 0);
        assert!(size >= raw);
        assert!(size - raw <= 8);
    }
}
