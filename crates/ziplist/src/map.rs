//! Sorted-pair scan: linear seek over adjacent-entry pairs (field/value for
//! hashes; member/score for sorted sets, Task 8), used by `hash.rs` (and
//! later `zset.rs`) to locate a key's pair or the sorted insertion point
//! for a new one.
//!
//! Bodies are sorted by the pair's *key* entry via [`compare`].
//! [`PairKey::First`] is the hash layout: the key is the even-indexed entry
//! of each `(field, value)` pair. [`pair_seek`] walks pairs from the head
//! and stops as soon as the current pair's key compares greater than the
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

/// Outcome of a [`pair_seek`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum SeekResult {
    /// An exact match: cursors onto the pair's key and value entries.
    Found { key_cur: Cursor, val_cur: Cursor },
    /// No exact match; the target belongs immediately before this pair's
    /// key entry to keep the body sorted.
    InsertBefore(Cursor),
    /// No exact match, and the target sorts after every existing pair (or
    /// the block is empty): it belongs at the tail.
    Tail,
}

/// Linear-scans `buf`'s `(key, value)` pairs for `key`, per `key_stride`'s
/// pairing convention. Bodies are sorted by key, so the scan exits as soon
/// as a pair's key compares greater than the target (early exit; no need
/// to walk the remainder).
///
/// Never panics: a decode failure or an unpaired trailing key entry (which
/// cannot happen on a block built exclusively through this crate's
/// pair-preserving ops, but is not re-validated here) is treated the same
/// as "nothing left to see" and reported as [`SeekResult::Tail`], mirroring
/// how [`crate::list::ListView::range`] treats an impossible-by-invariant
/// decode error as "nothing to walk" rather than panicking.
pub(crate) fn pair_seek(
    buf: &[u8],
    hdr: &BlockHeader,
    key: &EntryVal,
    key_stride: PairKey,
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
                return match cur.next(buf) {
                    Some(val_cur) => SeekResult::Found {
                        key_cur: cur,
                        val_cur,
                    },
                    None => SeekResult::Tail,
                };
            }
            Ordering::Greater => return SeekResult::InsertBefore(cur),
            Ordering::Less => {
                let val_cur = match cur.next(buf) {
                    Some(c) => c,
                    None => return SeekResult::Tail,
                };
                match val_cur.next(buf) {
                    Some(next_key) => cur = next_key,
                    None => return SeekResult::Tail,
                }
            }
        }
    }
}
