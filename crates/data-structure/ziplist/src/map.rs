//! Sorted-pair scan: linear seek over adjacent-entry pairs (field/value for
//! hashes; member/score for sorted sets) or single entries (set members),
//! used by `hash.rs` and `set.rs` to locate a key's pair/entry or the
//! sorted insertion point for a new one.
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
//!
//! # Zsets need a different seek: [`zset_seek`]
//!
//! `zset.rs`'s body is also `(member, score)` adjacent pairs, but its sort
//! key is `(score, member)` — a 2-tuple spanning *both* entries of the
//! pair, score primary, member the tiebreaker — not a single entry the way
//! [`PairKey::First`] assumes. `pair_seek`'s shape (its parameters,
//! [`PairKey`], [`Stride`]) is fixed by `hash.rs`/`set.rs`'s existing use,
//! so rather than bend it to fit a two-entry sort key, [`zset_seek`] is a
//! standalone sibling that walks pairs directly, comparing `(score,
//! member)` each step. It reuses [`SeekResult`]'s shape (see that
//! function's docs for how the fields map onto member/score cursors) so
//! `zset.rs`'s callers pattern-match it exactly like `pair_seek`'s result.
//! A zset's *member* lookups (score isn't known in advance) can't use
//! either seek — the body isn't member-sorted — and instead linear-scan
//! directly in `zset.rs`.

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

/// Linear-scans a zset body — `(member, score)` adjacent-entry pairs,
/// member first then score — for the pair sorting at `(score, member)`,
/// per the sort order `zset.rs` specifies: score ascending, ties broken by
/// `member` via [`compare`]. See the [module docs](self) for why this is a
/// standalone sibling of [`pair_seek`] rather than another `pair_seek`
/// call: the sort key here spans both entries of the pair, not just the
/// first.
///
/// Reuses [`SeekResult`]'s shape: on `Found`, `key_cur`/`val_cur` are the
/// matching pair's member/score cursors (the same mapping `pair_seek` uses
/// for [`Stride::Pair`]); `InsertBefore`/`Tail` mean exactly what they do
/// in `pair_seek`. Same never-panics contract too: a decode failure, a
/// score entry that isn't `EntryVal::Uint` (every zset score is written as
/// `Uint` by `zset.rs`; this is defensive, not expected), or a truncated
/// trailing unit all read as [`SeekResult::Tail`] rather than panicking.
pub(crate) fn zset_seek(
    buf: &[u8],
    hdr: &BlockHeader,
    score: u64,
    member: &EntryVal,
) -> SeekResult {
    let mut cur = match Cursor::first(buf, hdr) {
        Some(c) => c,
        None => return SeekResult::Tail,
    };
    loop {
        let member_cur = cur;
        let cur_member = match cur.value(buf) {
            Ok(v) => v,
            Err(_) => return SeekResult::Tail,
        };
        let score_cur = match cur.next(buf) {
            Some(c) => c,
            None => return SeekResult::Tail,
        };
        let cur_score = match score_cur.value(buf) {
            Ok(EntryVal::Uint(v)) => v,
            _ => return SeekResult::Tail,
        };
        match cur_score
            .cmp(&score)
            .then_with(|| compare(&cur_member, member))
        {
            Ordering::Equal => {
                return SeekResult::Found {
                    key_cur: member_cur,
                    val_cur: score_cur,
                };
            }
            Ordering::Greater => return SeekResult::InsertBefore(member_cur),
            Ordering::Less => match score_cur.next(buf) {
                Some(c) => cur = c,
                None => return SeekResult::Tail,
            },
        }
    }
}
