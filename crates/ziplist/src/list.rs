//! `ListView`/`ListMut`: Redis-list semantics over a `List`-typed block.
//!
//! A list's body is just the block's entry sequence in list order (index 0
//! at the head, `nentry - 1` at the tail); no extra pairing or sort
//! convention is layered on top the way `hash`/`set`/`zset` will need.
//! `index`/`range`/`trim` implement Redis's inclusive, negative-from-tail
//! index semantics: `i < 0` normalizes to `i + nentry` before use, and an
//! out-of-range `index` is `Ok(None)` rather than an error.
//!
//! # Type check happens at construction, not per call
//!
//! [`ListView::len`] returns a bare `u32` (no `Result`), so a `Type::List`
//! mismatch can't be surfaced there or in `index`/`range` without changing
//! their signatures. Instead [`ListView::parse`] validates
//! `hdr.type_ == Type::List` once, up front (mirroring `Block::parse`'s
//! "validate once, trust after" design), and every other `ListView` method
//! trusts that. [`ListMut::init`] sidesteps the question entirely: it
//! writes the empty block itself, so the type is `List` by construction.
//! [`ListMut::view`] therefore builds a `ListView` directly from the
//! already-trusted `BlockMut` state without re-parsing.
//!
//! # `pop_front`/`pop_back` take a callback, not a plain return
//!
//! A naive `pop_front(&mut self) -> Option<EntryVal>` can't be implemented
//! soundly here: `EntryVal::Str` borrows the block's backing bytes, but
//! removing the entry (`BlockMut::remove_at`) memmoves those same bytes
//! (`copy_within`), so any borrow of the popped entry's bytes must end
//! *before* the removal — yet the value has to be read *before* removal
//! too, since removal is what invalidates it. `forbid(unsafe_code)` and
//! `no_std` (no allocation to copy the bytes into) rule out the usual
//! escapes. A callback resolves this cleanly and with zero copies: the
//! popped `EntryVal` is handed to `f` while the borrow is still valid, and
//! only once `f` returns (and its borrow has ended) does the actual
//! `remove_at` run.
//!
//! `f` returns an owned `R`, not `()`: because the `EntryVal` parameter's
//! lifetime can't be tied to anything that survives the call (it's
//! necessarily elided/higher-ranked), a callback that returns nothing
//! would leave the caller unable to extract *anything* derived from the
//! value — not even an owned `u64` copied out of `EntryVal::Uint` — since
//! the closure body is the only place that lifetime is ever nameable.
//! Letting `f` return `R` (e.g. a copied `u64`, or `()` after writing the
//! bytes somewhere inside the closure, such as a protocol response buffer)
//! is what makes the callback actually usable.

use crate::block::{Block, BlockMut, InsertPos};
use crate::cursor::{locate, Cursor};
use crate::entry::EntryVal;
use crate::error::{DecodeError, Fit, NeedBytes};
use crate::header::{BlockHeader, Type};

/// A read-only view over a `List`-typed block, providing Redis list
/// semantics (0-based indexing, negative indices from the tail, inclusive
/// ranges) on top of the block's plain entry sequence.
#[derive(Debug, Clone, Copy)]
pub struct ListView<'a> {
    buf: &'a [u8],
    hdr: BlockHeader,
}

impl<'a> ListView<'a> {
    /// Parses `buf` as a list block: full structural validation
    /// ([`Block::parse`]) plus a `Type::List` check.
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let blk = Block::parse(buf)?;
        Self::from_block(blk)
    }

    fn from_block(blk: Block<'a>) -> Result<Self, DecodeError> {
        if blk.header().type_ != Type::List {
            return Err(DecodeError::Corrupt);
        }
        Ok(ListView {
            buf: blk.bytes(),
            hdr: *blk.header(),
        })
    }

    /// Number of entries in the list.
    pub fn len(&self) -> u32 {
        self.hdr.nentry
    }

    /// True iff the list has no entries.
    pub fn is_empty(&self) -> bool {
        self.hdr.nentry == 0
    }

    /// Normalizes a Redis-style index: negative values count from the tail
    /// (`-1` is the last entry). Does not clamp; callers check bounds.
    fn normalize(&self, i: i64) -> i64 {
        if i < 0 {
            i + self.hdr.nentry as i64
        } else {
            i
        }
    }

    /// Returns the entry at `i`, or `None` if `i` (after normalizing
    /// negative indices from the tail) is out of range.
    pub fn index(&self, i: i64) -> Result<Option<EntryVal<'a>>, DecodeError> {
        let nentry = self.hdr.nentry as i64;
        let idx = self.normalize(i);
        if idx < 0 || idx >= nentry {
            return Ok(None);
        }
        let cur = locate(self.buf, &self.hdr, idx as u32)?;
        Ok(Some(cur.value(self.buf)?))
    }

    /// Calls `f` with each entry in the inclusive range `[start, stop]`
    /// (Redis `LRANGE` semantics): negative indices normalize from the
    /// tail, out-of-range bounds clamp into `[0, nentry - 1]`, and an empty
    /// or fully out-of-range window (including `start > stop`) yields no
    /// calls at all.
    pub fn range(&self, start: i64, stop: i64, mut f: impl FnMut(&EntryVal<'a>)) {
        let nentry = self.hdr.nentry as i64;
        if nentry == 0 {
            return;
        }
        let start = self.normalize(start).max(0);
        let stop = self.normalize(stop).min(nentry - 1);
        if start > stop {
            return;
        }
        // 0 <= start <= stop <= nentry - 1, so `locate` cannot fail here;
        // treat an error as "nothing to walk" rather than panicking.
        let mut cur = match locate(self.buf, &self.hdr, start as u32) {
            Ok(c) => c,
            Err(_) => return,
        };
        let stop = stop as u32;
        let mut idx = start as u32;
        loop {
            if let Ok(v) = cur.value(self.buf) {
                f(&v);
            }
            if idx == stop {
                break;
            }
            idx += 1;
            match cur.next(self.buf) {
                Some(c) => cur = c,
                None => break,
            }
        }
    }
}

/// A mutable view over a `List`-typed block, providing push/pop/trim ops
/// with Redis list semantics.
#[derive(Debug)]
pub struct ListMut<'a> {
    blk: BlockMut<'a>,
}

impl<'a> ListMut<'a> {
    /// Initializes `buf` as an empty `List` block and wraps it. The type is
    /// `List` by construction, so no runtime type check is needed here or
    /// in any other `ListMut`/`ListView` op derived from it (see the
    /// [module docs](self)).
    pub fn init(buf: &'a mut [u8]) -> Result<Self, NeedBytes> {
        BlockHeader::init_empty(Type::List, buf)?;
        let blk =
            BlockMut::parse(buf).expect("a block just initialized as empty must parse cleanly");
        Ok(ListMut { blk })
    }

    /// A read-only view over the list's current contents.
    pub fn view(&self) -> ListView<'_> {
        ListView {
            buf: self.blk.bytes(),
            hdr: *self.blk.header(),
        }
    }

    /// The list's used bytes: header plus entries, through the end of the
    /// last entry. Same contract as
    /// [`BlockMut::bytes`](crate::block::BlockMut::bytes) (never the full
    /// backing capacity) -- exposed here so external callers (fuzzing,
    /// differential testing) can independently re-validate the block via
    /// [`Block::parse`](crate::block::Block::parse) without needing access
    /// to the private `BlockMut` this type wraps.
    pub fn bytes(&self) -> &[u8] {
        self.blk.bytes()
    }

    /// Prepends `val` to the front of the list (Redis `LPUSH`).
    pub fn push_front(&mut self, val: &EntryVal) -> Result<Fit, NeedBytes> {
        match Cursor::first(self.blk.bytes(), self.blk.header()) {
            Some(head) => self.blk.insert_at(InsertPos::Before(head), val),
            None => self.blk.insert_at(InsertPos::Tail, val),
        }
    }

    /// Appends `val` to the back of the list (Redis `RPUSH`).
    pub fn push_back(&mut self, val: &EntryVal) -> Result<Fit, NeedBytes> {
        self.blk.insert_at(InsertPos::Tail, val)
    }

    /// Removes the front entry, if any, passing it to `f` before removal
    /// (see the [module docs](self) for why this is a callback rather than
    /// a plain return). Returns `f`'s result, or `None` if the list was
    /// empty (in which case `f` is not called and nothing is removed).
    pub fn pop_front<R>(&mut self, f: impl FnOnce(EntryVal) -> R) -> Option<R> {
        let cur = Cursor::first(self.blk.bytes(), self.blk.header())?;
        let val = cur.value(self.blk.bytes()).ok()?;
        let r = f(val);
        self.blk.remove_at(cur);
        Some(r)
    }

    /// Removes the back entry, if any, passing it to `f` before removal.
    /// Same contract as [`ListMut::pop_front`], mirrored to the tail
    /// (Redis `RPOP`).
    pub fn pop_back<R>(&mut self, f: impl FnOnce(EntryVal) -> R) -> Option<R> {
        let cur = Cursor::last(self.blk.bytes(), self.blk.header())?;
        let val = cur.value(self.blk.bytes()).ok()?;
        let r = f(val);
        self.blk.remove_at(cur);
        Some(r)
    }

    /// Truncates the list to the inclusive window `[start, stop]` (Redis
    /// `LTRIM`): negative indices normalize from the tail, bounds clamp
    /// into range, and a `start > stop` window (after normalizing/clamping)
    /// empties the list. Implemented as two truncating `remove_at` loops
    /// from each end (O(removed) — fine at block scale); cursors are
    /// re-derived from the current block state on every iteration, per the
    /// crate-wide rule that ops never reuse a cursor across a mutation.
    pub fn trim(&mut self, start: i64, stop: i64) -> Fit {
        let nentry = self.blk.header().nentry as i64;
        if nentry == 0 {
            return Fit;
        }
        let normalize = |i: i64| if i < 0 { i + nentry } else { i };
        let start = normalize(start).max(0);
        let stop = normalize(stop).min(nentry - 1);

        if start > stop {
            while let Some(cur) = Cursor::first(self.blk.bytes(), self.blk.header()) {
                self.blk.remove_at(cur);
            }
            return Fit;
        }

        let mut remove_tail = nentry - 1 - stop;
        while remove_tail > 0 {
            if let Some(cur) = Cursor::last(self.blk.bytes(), self.blk.header()) {
                self.blk.remove_at(cur);
            }
            remove_tail -= 1;
        }

        let mut remove_front = start;
        while remove_front > 0 {
            if let Some(cur) = Cursor::first(self.blk.bytes(), self.blk.header()) {
                self.blk.remove_at(cur);
            }
            remove_front -= 1;
        }

        Fit
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::entry::EntryVal;
    use crate::list::{ListMut, ListView};
    use std::vec;
    use std::vec::Vec;

    fn s(b: &[u8]) -> EntryVal<'_> {
        EntryVal::Str(b)
    }

    fn collect<'a>(view: &ListView<'a>) -> Vec<EntryVal<'a>> {
        let mut out = Vec::new();
        for i in 0..view.len() {
            out.push(view.index(i as i64).unwrap().unwrap());
        }
        out
    }

    fn collect_range<'a>(view: &ListView<'a>, start: i64, stop: i64) -> Vec<EntryVal<'a>> {
        let mut out = Vec::new();
        view.range(start, stop, |v| out.push(*v));
        out
    }

    #[test]
    fn negative_indices_and_inclusive_range() {
        let mut buf = [0u8; 256];
        let mut l = ListMut::init(&mut buf).unwrap();
        for v in [b"a", b"b", b"c", b"d"] {
            l.push_back(&s(v)).unwrap();
        }
        assert_eq!(l.view().index(-1).unwrap(), Some(s(b"d")));
        assert_eq!(l.view().index(-5).unwrap(), None);
        assert_eq!(collect_range(&l.view(), 1, 2), vec![s(b"b"), s(b"c")]);
        assert_eq!(collect_range(&l.view(), -2, -1), vec![s(b"c"), s(b"d")]);
        assert_eq!(collect_range(&l.view(), 2, 0), Vec::<EntryVal>::new()); // start>stop
    }

    #[test]
    fn ltrim_keeps_inclusive_window() {
        let mut buf = [0u8; 256];
        let mut l = ListMut::init(&mut buf).unwrap();
        for v in 0u64..6 {
            l.push_back(&EntryVal::Uint(v)).unwrap();
        }
        l.trim(1, -2); // keep [1..=4]
        assert_eq!(
            collect(&l.view()),
            (1u64..=4).map(EntryVal::Uint).collect::<Vec<_>>()
        );
    }
}
