//! Sorted-pair scan: linear seek over adjacent-entry pairs (field/value for
//! hashes; member/score for sorted sets, Task 8) or single entries (set
//! members), used by `hash.rs` and `set.rs` (and later `zset.rs`) to locate
//! a key's pair/entry or the sorted insertion point for a new one.
//!
//! Bodies are sorted by the pair's *key* entry via [`compare`].
//! [`PairKey::First`] is the hash layout: the key is the even-indexed entry
//! of each `(field, value)` pair. [`Stride`] says how many physical entries
//! make up one logical unit: [`Stride::Pair`] advances two entries at a time
//! (hash: field, value); [`Stride::Single`] advances one at a time (a set
//! member has no paired value). [`pair_seek`] walks units from the head and
//! stops as soon as the current unit's key compares greater than the
//! target — since the body is sorted, everything after that point is
//! guaranteed greater too, so there is no need to keep scanning.

use crate::cursor::Cursor;
use crate::entry::{compare, EntryVal};
use crate::header::BlockHeader;
use core::cmp::Ordering;

/// Which entry of a `(key, value)` pair carries the sort/lookup key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairKey {
    /// The pair's first entry is the key (hash: field, value).
    First,
}

/// How many physical entries make up one logical unit of the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stride {
    /// Two adjacent entries per unit, key first (hash: field, value).
    Pair,
    /// One entry per unit (set: member only, no paired value). A set is
    /// the degenerate single-entry case of a pair-seek: see
    /// [`SeekResult::Found`]'s doc for how `val_cur` is handled here.
    Single,
}

/// Outcome of a [`pair_seek`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum SeekResult {
    /// An exact match: cursors onto the unit's key and value entries. For
    /// [`Stride::Single`] (no separate value entry exists), `val_cur`
    /// equals `key_cur` by convention, so this variant's shape doesn't need
    /// to change per stride — callers seeking a single-stride body (sets)
    /// simply ignore `val_cur`.
    Found { key_cur: Cursor, val_cur: Cursor },
    /// No exact match; the target belongs immediately before this unit's
    /// key entry to keep the body sorted.
    InsertBefore(Cursor),
    /// No exact match, and the target sorts after every existing unit (or
    /// the block is empty): it belongs at the tail.
    Tail,
}

/// Linear-scans `buf`'s body for `key`, per `key_stride`'s pairing
/// convention and `stride`'s unit size. Bodies are sorted by key, so the
/// scan exits as soon as a unit's key compares greater than the target
/// (early exit; no need to walk the remainder).
///
/// Never panics: a decode failure or an incomplete trailing unit (which
/// cannot happen on a block built exclusively through this crate's
/// stride-preserving ops, but is not re-validated here) is treated the same
/// as "nothing left to see" and reported as [`SeekResult::Tail`], mirroring
/// how [`crate::list::ListView::range`] treats an impossible-by-invariant
/// decode error as "nothing to walk" rather than panicking.
pub(crate) fn pair_seek(
    buf: &[u8],
    hdr: &BlockHeader,
    key: &EntryVal,
    key_stride: PairKey,
    stride: Stride,
) -> SeekResult {
    match key_stride {
        PairKey::First => {}
    }

    let mut cur = match Cursor::first(buf, hdr) {
        Some(c) => c,
        None => return SeekResult::Tail,
    };
    loop {
        let cur_key = match cur.value(buf) {
            Ok(v) => v,
            Err(_) => return SeekResult::Tail,
        };
        match compare(&cur_key, key) {
            Ordering::Equal => {
                return match stride {
                    Stride::Pair => match cur.next(buf) {
                        Some(val_cur) => SeekResult::Found {
                            key_cur: cur,
                            val_cur,
                        },
                        None => SeekResult::Tail,
                    },
                    Stride::Single => SeekResult::Found {
                        key_cur: cur,
                        val_cur: cur,
                    },
                };
            }
            Ordering::Greater => return SeekResult::InsertBefore(cur),
            Ordering::Less => {
                let next_key = match stride {
                    Stride::Pair => {
                        let val_cur = match cur.next(buf) {
                            Some(c) => c,
                            None => return SeekResult::Tail,
                        };
                        val_cur.next(buf)
                    }
                    Stride::Single => cur.next(buf),
                };
                match next_key {
                    Some(c) => cur = c,
                    None => return SeekResult::Tail,
                }
            }
        }
    }
}
