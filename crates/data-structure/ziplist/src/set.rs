//! `SetView`/`SetMut`: Redis-set semantics over a `Set`-typed block.
//!
//! A set's body is single entries (no pairing), sorted via [`compare`]. Each
//! member is classified through [`canonical_uint`] before being
//! stored/compared, the same rule
//! [`compare_raw`](crate::entry::compare_raw) and `hash.rs`'s `classify`
//! use: a canonical decimal rendering (e.g. `b"10"`) is stored/compared as
//! `EntryVal::Uint`, anything else (e.g. `b"01"`, which has a leading zero
//! and so is not canonical) as `EntryVal::Str`. This is what makes
//! `SADD k 10` and `SISMEMBER k 10` agree.
//!
//! Implemented as a thin wrapper over [`map::pair_seek`](crate::map) with
//! [`Stride::Single`]: the pair-seek machinery's "key" is the set's member,
//! and there is no separate value entry to track (`SeekResult::Found`'s
//! `val_cur` equals `key_cur` for this stride — see `map.rs`'s docs — and
//! is simply not read here).

use crate::block::{Block, BlockMut, InsertPos};
use crate::cursor::Cursor;
use crate::entry::{canonical_uint, EntryVal};
use crate::error::{DecodeError, Fit, NeedBytes};
use crate::header::{BlockHeader, Type};
use crate::map::{pair_seek, PairKey, SeekResult, Stride};

/// Classifies raw member bytes the same way
/// [`compare_raw`](crate::entry::compare_raw) does: a canonical decimal
/// renders as `EntryVal::Uint`, anything else as `EntryVal::Str`.
fn classify(bytes: &[u8]) -> EntryVal<'_> {
    canonical_uint(bytes).map_or(EntryVal::Str(bytes), EntryVal::Uint)
}

/// Result of [`SetMut::sadd`]: whether the member was newly added, or was
/// already present (Redis `SADD` semantics: adding an existing member is a
/// no-op that doesn't change cardinality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SAdd {
    /// The member did not exist; it was inserted.
    Added,
    /// The member already existed; nothing changed.
    AlreadyPresent,
}

/// A read-only view over a `Set`-typed block, providing Redis set semantics
/// (`SCARD`/`SISMEMBER`/member iteration) on top of the block's sorted,
/// single-entry body.
#[derive(Debug, Clone, Copy)]
pub struct SetView<'a> {
    buf: &'a [u8],
    hdr: BlockHeader,
}

impl<'a> SetView<'a> {
    /// Parses `buf` as a set block: full structural validation
    /// ([`Block::parse`]) plus a `Type::Set` check.
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let blk = Block::parse(buf)?;
        Self::from_block(blk)
    }

    fn from_block(blk: Block<'a>) -> Result<Self, DecodeError> {
        let hdr = *blk.header();
        if hdr.type_ != Type::Set {
            return Err(DecodeError::Corrupt);
        }
        Ok(SetView {
            buf: blk.bytes(),
            hdr,
        })
    }

    /// Number of members in the set (Redis `SCARD`); bare `nentry`, since
    /// unlike a hash's fields, a set member is a single entry with no
    /// paired value.
    pub fn scard(&self) -> u32 {
        self.hdr.nentry
    }

    /// True iff the set has no members.
    pub fn is_empty(&self) -> bool {
        self.hdr.nentry == 0
    }

    /// True iff `member` exists in the set (Redis `SISMEMBER`).
    pub fn sismember(&self, member: &[u8]) -> Result<bool, DecodeError> {
        let key = classify(member);
        match pair_seek(self.buf, &self.hdr, &key, PairKey::First, Stride::Single) {
            SeekResult::Found { .. } => Ok(true),
            _ => Ok(false),
        }
    }

    /// Calls `f` with each member, in comparator-sorted order (ints before
    /// strings, see [`compare`](crate::entry::compare)).
    pub fn iter_members(&self, mut f: impl FnMut(&EntryVal<'a>)) {
        let mut cur = match Cursor::first(self.buf, &self.hdr) {
            Some(c) => c,
            None => return,
        };
        loop {
            let member = match cur.value(self.buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            f(&member);
            cur = match cur.next(self.buf) {
                Some(c) => c,
                None => return,
            };
        }
    }
}

/// A mutable view over a `Set`-typed block, providing `SADD`/`SREM` ops
/// with Redis set semantics.
#[derive(Debug)]
pub struct SetMut<'a> {
    blk: BlockMut<'a>,
}

impl<'a> SetMut<'a> {
    /// Initializes `buf` as an empty `Set` block and wraps it. The type is
    /// `Set` by construction, so no runtime type check is needed here or in
    /// any other `SetMut`/`SetView` op derived from it.
    pub fn init(buf: &'a mut [u8]) -> Result<Self, NeedBytes> {
        BlockHeader::init_empty(Type::Set, buf)?;
        let blk =
            BlockMut::parse(buf).expect("a block just initialized as empty must parse cleanly");
        Ok(SetMut { blk })
    }

    /// A read-only view over the set's current contents.
    pub fn view(&self) -> SetView<'_> {
        SetView {
            buf: self.blk.bytes(),
            hdr: *self.blk.header(),
        }
    }

    /// The set's used bytes: header plus entries, through the end of the
    /// last entry. Same contract as
    /// [`BlockMut::bytes`](crate::block::BlockMut::bytes) (never the full
    /// backing capacity) -- exposed here so external callers (fuzzing,
    /// differential testing) can independently re-validate the block via
    /// [`Block::parse`](crate::block::Block::parse) without needing access
    /// to the private `BlockMut` this type wraps.
    pub fn bytes(&self) -> &[u8] {
        self.blk.bytes()
    }

    /// Adds `member` to the set (Redis `SADD`), returning whether it was
    /// newly added or already present. A single `insert_at` (no pairing to
    /// splice), so on `NeedBytes` the buffer is left untouched per
    /// `BlockMut::insert_at`'s own contract.
    pub fn sadd(&mut self, member: &[u8]) -> Result<SAdd, NeedBytes> {
        let key = classify(member);
        match pair_seek(
            self.blk.bytes(),
            self.blk.header(),
            &key,
            PairKey::First,
            Stride::Single,
        ) {
            SeekResult::Found { .. } => Ok(SAdd::AlreadyPresent),
            SeekResult::InsertBefore(cur) => {
                self.blk.insert_at(InsertPos::Before(cur), &key)?;
                Ok(SAdd::Added)
            }
            SeekResult::Tail => {
                self.blk.insert_at(InsertPos::Tail, &key)?;
                Ok(SAdd::Added)
            }
        }
    }

    /// Removes `member` (Redis `SREM`). Returns `None` if the member didn't
    /// exist (nothing removed).
    pub fn srem(&mut self, member: &[u8]) -> Option<Fit> {
        let key = classify(member);
        match pair_seek(
            self.blk.bytes(),
            self.blk.header(),
            &key,
            PairKey::First,
            Stride::Single,
        ) {
            SeekResult::Found { key_cur, .. } => Some(self.blk.remove_at(key_cur)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::entry::EntryVal;
    use crate::error::NeedBytes;
    use crate::set::{SAdd, SetMut, SetView};
    use std::vec;
    use std::vec::Vec;

    fn s(b: &[u8]) -> EntryVal<'_> {
        EntryVal::Str(b)
    }

    fn uint(v: u64) -> EntryVal<'static> {
        EntryVal::Uint(v)
    }

    fn members<'a>(view: &SetView<'a>) -> Vec<EntryVal<'a>> {
        let mut out = Vec::new();
        view.iter_members(|m| out.push(*m));
        out
    }

    #[test]
    fn sadd_twice_is_already_present_second_time_and_scard_stays_one() {
        let mut buf = [0u8; 256];
        let mut set = SetMut::init(&mut buf).unwrap();
        assert_eq!(set.sadd(b"member").unwrap(), SAdd::Added);
        assert_eq!(set.sadd(b"member").unwrap(), SAdd::AlreadyPresent);
        assert_eq!(set.view().scard(), 1);
    }

    #[test]
    fn membership_order_is_comparator_order() {
        let mut buf = [0u8; 256];
        let mut set = SetMut::init(&mut buf).unwrap();
        set.sadd(b"a").unwrap();
        set.sadd(b"10").unwrap();
        set.sadd(b"2").unwrap();
        // ints first (sorted numerically), then strings: [2, 10, "a"].
        assert_eq!(members(&set.view()), vec![uint(2), uint(10), s(b"a")]);
    }

    #[test]
    fn srem_miss_returns_none_hit_removes_member() {
        let mut buf = [0u8; 256];
        let mut set = SetMut::init(&mut buf).unwrap();
        set.sadd(b"m").unwrap();
        assert!(set.srem(b"missing").is_none());
        assert!(set.srem(b"m").is_some());
        assert!(!set.view().sismember(b"m").unwrap());
        assert_eq!(set.view().scard(), 0);
    }

    #[test]
    fn scard_tracks_adds_and_removes() {
        let mut buf = [0u8; 256];
        let mut set = SetMut::init(&mut buf).unwrap();
        set.sadd(b"a").unwrap();
        set.sadd(b"b").unwrap();
        set.sadd(b"c").unwrap();
        assert_eq!(set.view().scard(), 3);
        set.srem(b"b").unwrap();
        assert_eq!(set.view().scard(), 2);
    }

    #[test]
    fn sadd_needbytes_leaves_buffer_untouched() {
        let mut buf = [0u8; 16]; // 12-byte header + 4 spare bytes
        let mut set = SetMut::init(&mut buf).unwrap();
        assert_eq!(set.sadd(b"5").unwrap(), SAdd::Added); // canonical uint, 2 bytes, fits
        let before: Vec<u8> = set.blk.bytes_full().to_vec(); // snapshot whole buffer
        let big_member = b"0123456789"; // leading zero: not canonical, so Str; too big to fit
        let err = set.sadd(big_member).unwrap_err();
        assert!(matches!(err, NeedBytes(_)));
        assert_eq!(
            set.blk.bytes_full(),
            before.as_slice(),
            "failed sadd must not mutate"
        );
        assert_eq!(set.view().scard(), 1);
    }
}
