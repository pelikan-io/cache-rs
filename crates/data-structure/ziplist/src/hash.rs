//! `HashView`/`HashMut`: Redis-hash semantics over a `Hash`-typed block.
//!
//! A hash's body is `(field, value)` adjacent entry pairs, sorted by field
//! via [`compare`](crate::entry::compare). Only the *field* half of each
//! pair is classified through [`canonical_uint`] before being
//! stored/compared, the same rule [`compare_raw`](crate::entry::compare_raw)
//! uses: a canonical decimal rendering (e.g. `b"42"`) is stored/compared as
//! `EntryVal::Uint`, anything else as `EntryVal::Str`. This is what makes
//! `HSET k 42 v` and `HGET k 42` agree, and is also why integer fields
//! always sort before string fields ([`compare`](crate::entry::compare)'s
//! total order puts `Uint` before `Str`). The *value* half is stored
//! exactly as given, always `EntryVal::Str` — `HSET k f 9` followed by
//! `HGET k f` returns `EntryVal::Str(b"9")`, not `Uint(9)` — since values
//! are never compared or sorted, only [`HashMut::hincrby`] ever needs to
//! reinterpret one numerically, which it does on read via `canonical_uint`
//! without changing how values are written by `hset`.
//!
//! # `HashView::parse` also checks `nentry` is even
//!
//! [`Block::parse`] validates that every entry decodes cleanly, but knows
//! nothing about the hash-specific pairing convention layered on top: a
//! block with an odd `nentry` would leave a trailing, unpaired field with
//! no value. [`HashView::parse`] rejects that (`DecodeError::Corrupt`) so
//! every other method here can assume pairing holds and [`hlen`] (bare
//! `nentry / 2`, no `Result`) never silently truncates. [`HashMut::init`]
//! sidesteps the question by construction (starts at `nentry == 0`), and
//! every mutator here only ever adds or removes a pair as a unit, so the
//! invariant holds for the lifetime of a `HashMut` too.
//!
//! [`hlen`]: HashView::hlen

use crate::block::{Block, BlockMut, InsertPos};
use crate::cursor::Cursor;
use crate::entry::{canonical_uint, encoded_len, EntryVal};
use crate::error::{DecodeError, Fit, NeedBytes};
use crate::header::{BlockHeader, Type};
use crate::map::{pair_seek, PairKey, SeekResult, Stride};

/// Classifies raw field/value bytes the same way
/// [`compare_raw`](crate::entry::compare_raw) does: a canonical decimal
/// renders as `EntryVal::Uint`, anything else as `EntryVal::Str`.
fn classify(bytes: &[u8]) -> EntryVal<'_> {
    canonical_uint(bytes).map_or(EntryVal::Str(bytes), EntryVal::Uint)
}

/// In the `u64` domain, applies `delta` to `current`: a result below `0`
/// (including a negative `delta` applied on top of `0`) is `Underflow`; a
/// result past `u64::MAX` is `Overflow`.
///
/// `pub(crate)`: also used by `zset.rs`'s `zincrby`, which needs the exact
/// same `u64`-domain over/underflow rules (`HINCRBY`/`ZINCRBY` share this
/// arithmetic; only `hincrby`'s extra "not an integer" read differs, which
/// zset's always-`Uint` scores never need).
pub(crate) fn apply_delta(current: u64, delta: i64) -> Result<u64, IncrError> {
    if delta >= 0 {
        current.checked_add(delta as u64).ok_or(IncrError::Overflow)
    } else {
        current
            .checked_sub(delta.unsigned_abs())
            .ok_or(IncrError::Underflow)
    }
}

/// Result of [`HashMut::hset`]: whether the field was newly created, or an
/// existing field's value was overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HSet {
    /// The field did not exist; a new `(field, value)` pair was inserted.
    New,
    /// The field already existed; its value was overwritten.
    Updated,
}

/// A numeric error from [`HashMut::hincrby`], distinct from a capacity
/// failure (`NeedBytes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrError {
    /// The field's current value is not a canonical unsigned integer.
    NotAnInteger,
    /// Applying `delta` would take the value below `0`.
    Underflow,
    /// Applying `delta` would take the value past `u64::MAX`.
    Overflow,
}

/// A read-only view over a `Hash`-typed block, providing Redis hash
/// semantics (`HGET`/`HEXISTS`/`HLEN`/pair iteration) on top of the block's
/// sorted `(field, value)` pairs.
#[derive(Debug, Clone, Copy)]
pub struct HashView<'a> {
    buf: &'a [u8],
    hdr: BlockHeader,
}

impl<'a> HashView<'a> {
    /// Parses `buf` as a hash block: full structural validation
    /// ([`Block::parse`]) plus a `Type::Hash` check and an even-`nentry`
    /// check (see the [module docs](self)).
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let blk = Block::parse(buf)?;
        Self::from_block(blk)
    }

    fn from_block(blk: Block<'a>) -> Result<Self, DecodeError> {
        let hdr = *blk.header();
        if hdr.type_ != Type::Hash {
            return Err(DecodeError::Corrupt);
        }
        if !hdr.nentry.is_multiple_of(2) {
            return Err(DecodeError::Corrupt);
        }
        Ok(HashView {
            buf: blk.bytes(),
            hdr,
        })
    }

    /// Number of fields in the hash (`nentry / 2`, since each field is
    /// paired with a value entry).
    pub fn hlen(&self) -> u32 {
        self.hdr.nentry / 2
    }

    /// True iff the hash has no fields.
    pub fn is_empty(&self) -> bool {
        self.hdr.nentry == 0
    }

    /// Returns `field`'s value, or `None` if the field doesn't exist.
    pub fn hget(&self, field: &[u8]) -> Result<Option<EntryVal<'a>>, DecodeError> {
        let key = classify(field);
        match pair_seek(self.buf, &self.hdr, &key, PairKey::First, Stride::Pair) {
            SeekResult::Found { val_cur, .. } => Ok(Some(val_cur.value(self.buf)?)),
            _ => Ok(None),
        }
    }

    /// True iff `field` exists in the hash.
    pub fn hexists(&self, field: &[u8]) -> Result<bool, DecodeError> {
        Ok(self.hget(field)?.is_some())
    }

    /// Calls `f` with each `(field, value)` pair, in field-sorted order.
    pub fn iter_pairs(&self, mut f: impl FnMut(&EntryVal<'a>, &EntryVal<'a>)) {
        let mut cur = match Cursor::first(self.buf, &self.hdr) {
            Some(c) => c,
            None => return,
        };
        loop {
            let field = match cur.value(self.buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            let val_cur = match cur.next(self.buf) {
                Some(c) => c,
                None => return,
            };
            let value = match val_cur.value(self.buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            f(&field, &value);
            cur = match val_cur.next(self.buf) {
                Some(c) => c,
                None => return,
            };
        }
    }
}

/// A mutable view over a `Hash`-typed block, providing `HSET`/`HDEL`/
/// `HINCRBY` ops with Redis hash semantics.
#[derive(Debug)]
pub struct HashMut<'a> {
    blk: BlockMut<'a>,
}

impl<'a> HashMut<'a> {
    /// Initializes `buf` as an empty `Hash` block and wraps it. The type is
    /// `Hash` and `nentry` starts at `0` (even) by construction, so no
    /// runtime check is needed here or in any other `HashMut`/`HashView`
    /// op derived from it (see the [module docs](self)).
    pub fn init(buf: &'a mut [u8]) -> Result<Self, NeedBytes> {
        BlockHeader::init_empty(Type::Hash, buf)?;
        let blk =
            BlockMut::parse(buf).expect("a block just initialized as empty must parse cleanly");
        Ok(HashMut { blk })
    }

    /// A read-only view over the hash's current contents.
    pub fn view(&self) -> HashView<'_> {
        HashView {
            buf: self.blk.bytes(),
            hdr: *self.blk.header(),
        }
    }

    /// The hash's used bytes: header plus entries, through the end of the
    /// last entry. Same contract as
    /// [`BlockMut::bytes`](crate::block::BlockMut::bytes) (never the full
    /// backing capacity) -- exposed here so external callers (fuzzing,
    /// differential testing) can independently re-validate the block via
    /// [`Block::parse`](crate::block::Block::parse) without needing access
    /// to the private `BlockMut` this type wraps.
    pub fn bytes(&self) -> &[u8] {
        self.blk.bytes()
    }

    /// Sets `field` to `value` (Redis `HSET`), returning whether the field
    /// was newly created or an existing one was overwritten.
    ///
    /// The update path (`HSet::Updated`) is a single `replace_at` on the
    /// value entry: on `NeedBytes`, the exact total is `used_len -
    /// old_val_len + new_val_len` (`BlockMut::replace_at`'s own contract),
    /// and the buffer is left untouched. The insert path (`HSet::New`)
    /// splices two adjacent entries (field, then value) and is
    /// all-or-nothing: see [`insert_pair`](Self::insert_pair).
    pub fn hset(&mut self, field: &[u8], value: &[u8]) -> Result<HSet, NeedBytes> {
        let key = classify(field);
        // Only the field is classified/sorted; the value is stored exactly
        // as given (a canonical-looking value like `b"9"` round-trips
        // through `hget` as `EntryVal::Str(b"9")`, not `Uint(9)`) — see the
        // [module docs](self).
        let val = EntryVal::Str(value);
        match pair_seek(
            self.blk.bytes(),
            self.blk.header(),
            &key,
            PairKey::First,
            Stride::Pair,
        ) {
            SeekResult::Found { val_cur, .. } => {
                self.blk.replace_at(val_cur, &val)?;
                Ok(HSet::Updated)
            }
            SeekResult::InsertBefore(cur) => {
                self.insert_pair(InsertPos::Before(cur), &key, &val)?;
                Ok(HSet::New)
            }
            SeekResult::Tail => {
                self.insert_pair(InsertPos::Tail, &key, &val)?;
                Ok(HSet::New)
            }
        }
    }

    /// Removes `field` and its value (Redis `HDEL`). Returns `None` if the
    /// field didn't exist (nothing removed).
    pub fn hdel(&mut self, field: &[u8]) -> Option<Fit> {
        let key = classify(field);
        match pair_seek(
            self.blk.bytes(),
            self.blk.header(),
            &key,
            PairKey::First,
            Stride::Pair,
        ) {
            SeekResult::Found { key_cur, val_cur } => {
                // val_cur always sits at a higher offset than key_cur
                // (adjacent pair, value follows field): removing it first
                // doesn't shift key_cur's offset. Removing key_cur first
                // would shift-invalidate val_cur before its own removal.
                self.blk.remove_at(val_cur);
                self.blk.remove_at(key_cur);
                Some(Fit)
            }
            _ => None,
        }
    }

    /// Increments `field`'s value by `delta` (Redis `HINCRBY`), storing the
    /// result re-encoded. A missing field starts at `0` (Redis semantics).
    /// The outer `Result` is capacity (`NeedBytes`, from either the
    /// existing-field `replace_at` — the new value's tier may need more
    /// bytes than the old one — or a new pair's [`insert_pair`](Self::insert_pair));
    /// the inner `Result` is the numeric outcome.
    pub fn hincrby(
        &mut self,
        field: &[u8],
        delta: i64,
    ) -> Result<Result<u64, IncrError>, NeedBytes> {
        let key = classify(field);
        match pair_seek(
            self.blk.bytes(),
            self.blk.header(),
            &key,
            PairKey::First,
            Stride::Pair,
        ) {
            SeekResult::Found { val_cur, .. } => {
                let current = val_cur
                    .value(self.blk.bytes())
                    .expect("cursor just produced by pair_seek over the current, unmutated block");
                let current = match current {
                    EntryVal::Uint(v) => v,
                    EntryVal::Str(s) => match canonical_uint(s) {
                        Some(v) => v,
                        None => return Ok(Err(IncrError::NotAnInteger)),
                    },
                };
                let new_val = match apply_delta(current, delta) {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                };
                self.blk.replace_at(val_cur, &EntryVal::Uint(new_val))?;
                Ok(Ok(new_val))
            }
            SeekResult::InsertBefore(cur) => {
                self.hincrby_absent(InsertPos::Before(cur), &key, delta)
            }
            SeekResult::Tail => self.hincrby_absent(InsertPos::Tail, &key, delta),
        }
    }

    /// `hincrby` on a field that doesn't yet exist: applies `delta` to a
    /// starting value of `0` and, if that doesn't under/overflow, inserts
    /// the new `(field, value)` pair via [`insert_pair`](Self::insert_pair).
    fn hincrby_absent(
        &mut self,
        pos: InsertPos,
        key: &EntryVal,
        delta: i64,
    ) -> Result<Result<u64, IncrError>, NeedBytes> {
        let new_val = match apply_delta(0, delta) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        self.insert_pair(pos, key, &EntryVal::Uint(new_val))?;
        Ok(Ok(new_val))
    }

    /// Inserts `key`/`val` as an adjacent pair at `pos` (field first, then
    /// value), all-or-nothing: the combined size is checked against
    /// capacity *before* either splice runs, so a `NeedBytes` failure
    /// leaves the buffer byte-for-byte unmodified. This extends each
    /// individual `BlockMut` splice's own untouched-on-failure guarantee
    /// across the two splices a pair-insert requires.
    fn insert_pair(
        &mut self,
        pos: InsertPos,
        key: &EntryVal,
        val: &EntryVal,
    ) -> Result<Fit, NeedBytes> {
        let klen = encoded_len(key);
        let vlen = encoded_len(val);
        let capacity = self.blk.bytes_full().len();
        let need = klen
            .checked_add(vlen)
            .and_then(|n| self.blk.used_len().checked_add(n));
        match need {
            Some(total) if total <= capacity => {}
            Some(total) => return Err(NeedBytes(total)),
            None => return Err(NeedBytes(usize::MAX)),
        }

        // Place the field, then splice the value immediately after it.
        // `InsertPos::Before(cur)` inserts at `cur.off`, so once the field
        // lands there, whatever used to be at `cur.off` (if anything) is
        // now at `cur.off + klen` -- exactly the value's insertion point.
        // Neither `insert_at` call below can fail: capacity was already
        // confirmed for their sum above.
        match pos {
            InsertPos::Before(cur) => {
                self.blk
                    .insert_at(InsertPos::Before(cur), key)
                    .expect("capacity already confirmed to fit both entries");
                let shifted = Cursor {
                    off: cur.off + klen,
                    len: cur.len,
                };
                self.blk
                    .insert_at(InsertPos::Before(shifted), val)
                    .expect("capacity already confirmed to fit both entries");
            }
            InsertPos::Tail => {
                self.blk
                    .insert_at(InsertPos::Tail, key)
                    .expect("capacity already confirmed to fit both entries");
                self.blk
                    .insert_at(InsertPos::Tail, val)
                    .expect("capacity already confirmed to fit both entries");
            }
        }
        Ok(Fit)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::entry::EntryVal;
    use crate::hash::{HSet, HashMut, HashView, IncrError};
    use std::vec;
    use std::vec::Vec;

    fn s(b: &[u8]) -> EntryVal<'_> {
        EntryVal::Str(b)
    }

    fn uint(v: u64) -> EntryVal<'static> {
        EntryVal::Uint(v)
    }

    fn fields<'a>(view: &HashView<'a>) -> Vec<EntryVal<'a>> {
        let mut out = Vec::new();
        view.iter_pairs(|k, _v| out.push(*k));
        out
    }

    #[test]
    fn hset_maintains_field_sorted_order_and_pairing() {
        let mut buf = [0u8; 512];
        let mut h = HashMut::init(&mut buf).unwrap();
        assert!(matches!(h.hset(b"zebra", b"1").unwrap(), HSet::New));
        assert!(matches!(h.hset(b"apple", b"2").unwrap(), HSet::New));
        assert!(matches!(h.hset(b"10", b"3").unwrap(), HSet::New)); // int field
        assert!(matches!(h.hset(b"apple", b"9").unwrap(), HSet::Updated));
        // ints first, then strings lexicographic:
        assert_eq!(fields(&h.view()), vec![uint(10), s(b"apple"), s(b"zebra")]);
        assert_eq!(h.view().hget(b"apple").unwrap(), Some(s(b"9")));
        assert_eq!(h.view().hlen(), 3);
    }

    #[test]
    fn hdel_and_miss() {
        let mut buf = [0u8; 256];
        let mut h = HashMut::init(&mut buf).unwrap();
        h.hset(b"f", b"v").unwrap();
        assert!(h.hdel(b"f").is_some());
        assert!(h.hdel(b"f").is_none());
        assert_eq!(h.view().hget(b"f").unwrap(), None);
        assert_eq!(h.view().hlen(), 0);
    }

    #[test]
    fn hincrby_on_numeric_and_error_on_string() {
        let mut buf = [0u8; 256];
        let mut h = HashMut::init(&mut buf).unwrap();
        h.hset(b"n", b"10").unwrap();
        assert_eq!(h.hincrby(b"n", 5).unwrap(), Ok(15));
        h.hset(b"strv", b"abc").unwrap();
        assert_eq!(h.hincrby(b"strv", 1).unwrap(), Err(IncrError::NotAnInteger));
        assert_eq!(h.hincrby(b"n", -100).unwrap(), Err(IncrError::Underflow)); // u64 floor
    }
}
