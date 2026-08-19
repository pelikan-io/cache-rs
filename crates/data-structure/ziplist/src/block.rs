//! Block splice primitives: parsing whole blocks and mutating them in place.
//!
//! [`Block`] is a read-only, validated view over a byte slice: `parse` walks
//! every entry once (bounded by `nentry`), checks it against `tail_off`, and
//! derives the block's used length so that every later read trusts the
//! parsed state instead of re-validating.
//!
//! [`BlockMut`] is the mutable counterpart: it wraps a `&mut [u8]` whose
//! length is the block's *capacity*, tracks the used length separately, and
//! provides `insert_at`/`remove_at`/`replace_at`, all funneled through a
//! single private `splice` that does the `copy_within` memmove and keeps
//! `nentry`/`tail_off` consistent.
//!
//! Per the [`cursor`](crate::cursor) module's contract, every cursor/locate
//! call must see a buffer sliced to the block's *used* length, never full
//! capacity — that's exactly what [`Block::bytes`] and [`BlockMut::bytes`]
//! return. [`BlockMut::bytes_full`] exposes the raw capacity view, needed
//! only to snapshot the whole backing buffer (e.g. to assert a failed op
//! left it untouched).

use crate::cursor::Cursor;
use crate::entry::{decode, decode_backward, encode_into, encoded_len, EntryVal};
use crate::error::{DecodeError, Fit, NeedBytes};
use crate::header::{BlockHeader, HEADER_SIZE};

/// Walks a block's entries from `HEADER_SIZE` to `hdr.tail_off`, validating
/// that exactly `hdr.nentry` entries are found along the way and that the
/// last one (at `tail_off`) decodes cleanly. Returns the parsed header and
/// the block's used length (`tail_off` + the last entry's encoded length).
///
/// `buf` may be longer than the used length (e.g. a `BlockMut`'s full
/// capacity); this only validates and measures the used prefix.
fn validate(buf: &[u8]) -> Result<(BlockHeader, usize), DecodeError> {
    let hdr = BlockHeader::parse(buf)?;
    let tail_off = hdr.tail_off as usize;

    if hdr.nentry == 0 {
        if tail_off != HEADER_SIZE {
            return Err(DecodeError::Corrupt);
        }
        return Ok((hdr, HEADER_SIZE));
    }
    if tail_off < HEADER_SIZE {
        return Err(DecodeError::Corrupt);
    }

    let mut off = HEADER_SIZE;
    let mut count: u32 = 0;
    while off != tail_off {
        if off > tail_off {
            return Err(DecodeError::Corrupt);
        }
        let (_, len) = decode(buf, off)?;
        off = off.checked_add(len).ok_or(DecodeError::Corrupt)?;
        count = count.checked_add(1).ok_or(DecodeError::Corrupt)?;
    }
    let (_, last_len) = decode(buf, tail_off)?;
    count = count.checked_add(1).ok_or(DecodeError::Corrupt)?;
    if count != hdr.nentry {
        return Err(DecodeError::Corrupt);
    }
    let used_len = tail_off.checked_add(last_len).ok_or(DecodeError::Corrupt)?;
    if used_len > buf.len() {
        return Err(DecodeError::Truncated);
    }
    Ok((hdr, used_len))
}

/// A validated, read-only view over a ziplist block.
#[derive(Debug, Clone, Copy)]
pub struct Block<'a> {
    buf: &'a [u8],
    hdr: BlockHeader,
    len: usize,
}

impl<'a> Block<'a> {
    /// Parses and fully validates a block: walks every entry (bounded by
    /// the header's `nentry`), checks it against `tail_off`, and derives
    /// the used length. `buf` may be longer than the used length.
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let (hdr, len) = validate(buf)?;
        Ok(Block { buf, hdr, len })
    }

    /// The block's header.
    pub fn header(&self) -> &BlockHeader {
        &self.hdr
    }

    /// The block's used bytes: header plus entries, through the end of the
    /// entry at `tail_off`. This is the slice every cursor/locate call must
    /// see (see the [module docs](self)).
    pub fn bytes(&self) -> &'a [u8] {
        &self.buf[..self.len]
    }

    /// Number of used bytes (header + entries).
    pub fn used_len(&self) -> usize {
        self.len
    }
}

/// Where to insert a new entry relative to an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPos {
    /// Insert immediately before the entry the cursor points at. The
    /// cursor MUST come from the block's current state (see
    /// [`BlockMut`]'s docs).
    Before(Cursor),
    /// Insert after the last entry (or as the only entry, if empty).
    Tail,
}

/// A validated, mutable view over a ziplist block backed by a fixed-size
/// buffer. The buffer's length is the block's capacity; `used_len()` is how
/// much of it is currently occupied by the header and entries.
///
/// Every mutation shifts offsets: a [`Cursor`] obtained before a mutation
/// MUST NOT be passed to any later op — re-derive cursors from the current
/// state. A stale cursor's offsets are not detected and can splice at the
/// wrong position or panic on an out-of-range `copy_within`.
#[derive(Debug)]
pub struct BlockMut<'a> {
    buf: &'a mut [u8],
    hdr: BlockHeader,
    len: usize,
}

impl<'a> BlockMut<'a> {
    /// Parses and fully validates the used prefix of `buf`, same as
    /// [`Block::parse`]. `buf.len()` becomes the block's capacity for
    /// subsequent inserts/replaces.
    pub fn parse(buf: &'a mut [u8]) -> Result<Self, DecodeError> {
        let (hdr, len) = validate(buf)?;
        Ok(BlockMut { buf, hdr, len })
    }

    /// The block's header.
    pub fn header(&self) -> &BlockHeader {
        &self.hdr
    }

    /// The block's used bytes: header plus entries, through the end of the
    /// entry at `tail_off`. This is the slice every cursor/locate call must
    /// see (see the [module docs](self)); it is never the full capacity.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// The full backing buffer, including any unused spare capacity past
    /// the used length. Only for snapshotting/inspecting the raw storage
    /// (e.g. asserting a failed op left it byte-for-byte unchanged); never
    /// pass this to cursor/locate (see the [module docs](self)).
    pub fn bytes_full(&self) -> &[u8] {
        self.buf
    }

    /// Number of used bytes (header + entries).
    pub fn used_len(&self) -> usize {
        self.len
    }

    /// Inserts `val` at `pos`. On success, `nentry` and `tail_off` are
    /// updated and the header is rewritten. On failure, returns the exact
    /// total block length (`used_len() + encoded_len(val)`) that would be
    /// needed, and leaves the buffer byte-for-byte unmodified.
    pub fn insert_at(&mut self, pos: InsertPos, val: &EntryVal) -> Result<Fit, NeedBytes> {
        let off = match pos {
            InsertPos::Before(cur) => cur.off,
            InsertPos::Tail => self.len,
        };
        self.splice(off, 0, Some(val))
    }

    /// Removes the entry at `cur`. Removal always fits (it can only shrink
    /// the block), so this cannot fail. `cur` MUST come from the block's
    /// current state (see the type docs).
    pub fn remove_at(&mut self, cur: Cursor) -> Fit {
        self.splice(cur.off, cur.len, None)
            .expect("removal cannot need more bytes than it frees")
    }

    /// Replaces the entry at `cur` with `val`. On failure, returns the
    /// exact total block length needed and leaves the buffer unmodified.
    /// `cur` MUST come from the block's current state (see the type docs).
    pub fn replace_at(&mut self, cur: Cursor, val: &EntryVal) -> Result<Fit, NeedBytes> {
        self.splice(cur.off, cur.len, Some(val))
    }

    /// Splices `new_len` bytes (encoding `val`, or none if `val` is `None`)
    /// in place of the `old_len` bytes at `off`, handling insert
    /// (`old_len == 0`), remove (`val.is_none()`), and replace (both
    /// nonzero/`Some`) uniformly. Moves the unaffected tail via
    /// `copy_within`, then maintains `nentry`/`tail_off` and rewrites the
    /// header. Bails out with `NeedBytes` *before* touching the buffer if
    /// the result wouldn't fit in capacity.
    fn splice(
        &mut self,
        off: usize,
        old_len: usize,
        val: Option<&EntryVal>,
    ) -> Result<Fit, NeedBytes> {
        let used = self.len;
        let new_len = val.map_or(0, encoded_len);

        let new_used = used
            .checked_sub(old_len)
            .and_then(|v| v.checked_add(new_len));
        let new_used = match new_used {
            Some(nu) if nu <= self.buf.len() => nu,
            Some(nu) => return Err(NeedBytes(nu)),
            None => return Err(NeedBytes(usize::MAX)),
        };

        let old_tail_off = self.hdr.tail_off as usize;
        let old_nentry = self.hdr.nentry;

        let (new_tail_off, new_nentry) = if val.is_some() && old_len == 0 {
            // Insert.
            let new_tail_off = if old_nentry == 0 || off > old_tail_off {
                // Empty block, or inserting after the current tail: the new
                // entry becomes (or starts as) the tail.
                off
            } else {
                // Inserting at or before the current tail: it shifts right.
                old_tail_off
                    .checked_add(new_len)
                    .expect("tail_off overflow")
            };
            let new_nentry = old_nentry.checked_add(1).expect("nentry overflow");
            (new_tail_off, new_nentry)
        } else if val.is_none() {
            // Remove.
            let new_tail_off = if off == old_tail_off {
                if old_nentry == 1 {
                    HEADER_SIZE
                } else {
                    decode_backward(&self.buf[..used], off).expect("prior entry must be valid")
                }
            } else {
                old_tail_off
                    .checked_sub(old_len)
                    .expect("tail_off underflow")
            };
            let new_nentry = old_nentry.checked_sub(1).expect("nentry underflow");
            (new_tail_off, new_nentry)
        } else {
            // Replace.
            let new_tail_off = if off == old_tail_off {
                // Same entry, in place; only its length may have changed.
                off
            } else {
                old_tail_off
                    .checked_add(new_len)
                    .and_then(|v| v.checked_sub(old_len))
                    .expect("tail_off arithmetic")
            };
            (new_tail_off, old_nentry)
        };

        let old_end = off.checked_add(old_len).expect("old_end overflow");
        if new_len != old_len {
            self.buf.copy_within(old_end..used, off + new_len);
        }
        if let Some(v) = val {
            encode_into(v, &mut self.buf[off..off + new_len]);
        }

        self.len = new_used;
        self.hdr.tail_off = new_tail_off as u32;
        self.hdr.nentry = new_nentry;
        self.hdr.write_to(&mut self.buf[..HEADER_SIZE]);

        Ok(Fit)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::block::{Block, BlockMut, InsertPos};
    use crate::cursor::{locate, Cursor};
    use crate::entry::{encoded_len, EntryVal};
    use crate::header::{BlockHeader, Type};
    use std::{vec, vec::Vec};

    #[derive(Debug, PartialEq)]
    enum Owned {
        Uint(u64),
        Str(Vec<u8>),
    }

    fn uint(v: u64) -> Owned {
        Owned::Uint(v)
    }

    fn s(b: &[u8]) -> Owned {
        Owned::Str(b.to_vec())
    }

    fn collect(blk: &BlockMut) -> Vec<Owned> {
        let mut out = Vec::new();
        let mut cur = Cursor::first(blk.bytes(), blk.header());
        while let Some(c) = cur {
            out.push(match c.value(blk.bytes()).unwrap() {
                EntryVal::Uint(x) => Owned::Uint(x),
                EntryVal::Str(bytes) => Owned::Str(bytes.to_vec()),
            });
            cur = c.next(blk.bytes());
        }
        out
    }

    #[test]
    fn insert_head_mid_tail_then_remove_all() {
        let mut buf = [0u8; 256];
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        let mut blk = BlockMut::parse(&mut buf).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Str(b"b"))
            .unwrap(); // [b]
        let head = Cursor::first(blk.bytes(), blk.header()).unwrap();
        blk.insert_at(InsertPos::Before(head), &EntryVal::Uint(1))
            .unwrap(); // [1,b]
        blk.insert_at(InsertPos::Tail, &EntryVal::Str(b"c"))
            .unwrap(); // [1,b,c]
        assert_eq!(collect(&blk), vec![uint(1), s(b"b"), s(b"c")]);
        let mid = locate(blk.bytes(), blk.header(), 1).unwrap();
        blk.remove_at(mid); // [1,c]
        assert_eq!(collect(&blk), vec![uint(1), s(b"c")]);
        assert_eq!(blk.header().nentry, 2);
    }

    #[test]
    fn insert_beyond_capacity_reports_exact_need_and_mutates_nothing() {
        let mut buf = [0u8; 16]; // 12 header + 4 spare
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        let mut blk = BlockMut::parse(&mut buf).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Uint(5)).unwrap(); // 2 bytes, fits
        let before: Vec<u8> = blk.bytes_full().to_vec(); // snapshot whole buffer
        let big = EntryVal::Str(b"0123456789");
        let need = blk.insert_at(InsertPos::Tail, &big).unwrap_err();
        assert_eq!(need.0, blk.used_len() + encoded_len(&big));
        assert_eq!(
            blk.bytes_full(),
            before.as_slice(),
            "failed insert must not mutate"
        );
    }

    #[test]
    fn parse_rejects_inconsistent_nentry() {
        let mut buf = [0u8; 64];
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        let mut blk = BlockMut::parse(&mut buf).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Uint(1)).unwrap();
        let used = blk.used_len();
        buf[4..8].copy_from_slice(&7u32.to_le_bytes()); // lie about nentry
        assert!(Block::parse(&buf[..used]).is_err());
    }

    #[test]
    fn replace_at_keeps_header_consistent_for_tail_and_non_tail() {
        let mut buf = [0u8; 128];
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        let mut blk = BlockMut::parse(&mut buf).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Uint(1)).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Str(b"xx"))
            .unwrap();
        // [1, xx]
        let head = Cursor::first(blk.bytes(), blk.header()).unwrap();
        blk.replace_at(head, &EntryVal::Str(b"longer-value"))
            .unwrap();
        // [longer-value, xx], non-tail replace grew: tail_off must shift.
        let tail = Cursor::last(blk.bytes(), blk.header()).unwrap();
        blk.replace_at(tail, &EntryVal::Uint(99)).unwrap();
        // [longer-value, 99], tail replace: tail_off stays put.
        assert_eq!(collect(&blk), vec![s(b"longer-value"), uint(99)]);
        assert_eq!(blk.header().nentry, 2);
        // A full re-validation confirms nentry/tail_off/used_len are consistent.
        assert!(Block::parse(blk.bytes()).is_ok());
    }

    #[test]
    fn replace_beyond_capacity_reports_exact_need_and_mutates_nothing() {
        let mut buf = [0u8; 20]; // 12 header + 8 spare
        BlockHeader::init_empty(Type::List, &mut buf).unwrap();
        let mut blk = BlockMut::parse(&mut buf).unwrap();
        blk.insert_at(InsertPos::Tail, &EntryVal::Uint(5)).unwrap(); // 2 bytes, fits
        let cur = Cursor::first(blk.bytes(), blk.header()).unwrap();
        let before: Vec<u8> = blk.bytes_full().to_vec();
        let big = EntryVal::Str(b"0123456789");
        let need = blk.replace_at(cur, &big).unwrap_err();
        assert_eq!(need.0, blk.used_len() - cur.len + encoded_len(&big));
        assert_eq!(
            blk.bytes_full(),
            before.as_slice(),
            "failed replace must not mutate"
        );
    }
}
