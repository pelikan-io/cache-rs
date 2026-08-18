//! Cursor-based traversal over ziplist entries.
//!
//! A [`Cursor`] points at a single entry within a block and can walk
//! forward or backward one entry at a time. [`locate`] jumps directly to
//! the `idx`-th entry by walking from whichever end of the block is
//! nearer, so index lookups cost at most `nentry / 2` steps.

use crate::entry::{decode, decode_backward, EntryVal};
use crate::error::DecodeError;
use crate::header::{BlockHeader, HEADER_SIZE};

/// A position within a ziplist block, pointing at one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Offset of the entry's first byte (its tag).
    pub off: usize,
    /// Total length of the entry, including its backlen.
    pub len: usize,
}

impl Cursor {
    /// Returns a cursor to the first entry in the block, or `None` if the
    /// block is empty.
    pub fn first(buf: &[u8], hdr: &BlockHeader) -> Option<Cursor> {
        if hdr.nentry == 0 {
            return None;
        }
        Self::at(buf, HEADER_SIZE)
    }

    /// Returns a cursor to the last entry in the block, or `None` if the
    /// block is empty.
    pub fn last(buf: &[u8], hdr: &BlockHeader) -> Option<Cursor> {
        if hdr.nentry == 0 {
            return None;
        }
        Self::at(buf, hdr.tail_off as usize)
    }

    /// Builds a cursor at `off` by decoding the entry there, which
    /// re-validates it in the process. Returns `None` if `off` doesn't
    /// hold a valid entry.
    fn at(buf: &[u8], off: usize) -> Option<Cursor> {
        let (_, len) = decode(buf, off).ok()?;
        Some(Cursor { off, len })
    }

    /// Returns a cursor to the entry following this one, or `None` if this
    /// is the last entry (walking off the end of the block, or of the
    /// buffer, both surface as a decode failure at the next offset).
    pub fn next(&self, buf: &[u8]) -> Option<Cursor> {
        let next_off = self.off.checked_add(self.len)?;
        Self::at(buf, next_off)
    }

    /// Returns a cursor to the entry preceding this one, or `None` if this
    /// is the first entry.
    pub fn prev(&self, buf: &[u8], hdr: &BlockHeader) -> Option<Cursor> {
        if self.off <= HEADER_SIZE || self.off as u32 > hdr.tail_off {
            return None;
        }
        let prev_off = decode_backward(buf, self.off).ok()?;
        Self::at(buf, prev_off)
    }

    /// Decodes and returns the value at this cursor's position.
    pub fn value<'a>(&self, buf: &'a [u8]) -> Result<EntryVal<'a>, DecodeError> {
        let (val, _) = decode(buf, self.off)?;
        Ok(val)
    }
}

/// Locates the `idx`-th entry (0-based) in the block, walking from
/// whichever end of the block is nearer: forward from the first entry if
/// `idx < nentry / 2`, otherwise backward from the last entry.
///
/// Returns an error if `idx >= hdr.nentry`, or if the walk encounters a
/// corrupt or truncated entry along the way.
pub fn locate(buf: &[u8], hdr: &BlockHeader, idx: u32) -> Result<Cursor, DecodeError> {
    if idx >= hdr.nentry {
        return Err(DecodeError::Corrupt);
    }
    if idx < hdr.nentry / 2 {
        let mut cur = Cursor::first(buf, hdr).ok_or(DecodeError::Corrupt)?;
        for _ in 0..idx {
            cur = cur.next(buf).ok_or(DecodeError::Corrupt)?;
        }
        Ok(cur)
    } else {
        let mut cur = Cursor::last(buf, hdr).ok_or(DecodeError::Corrupt)?;
        let steps = hdr
            .nentry
            .checked_sub(1)
            .and_then(|n| n.checked_sub(idx))
            .ok_or(DecodeError::Corrupt)?;
        for _ in 0..steps {
            cur = cur.prev(buf, hdr).ok_or(DecodeError::Corrupt)?;
        }
        Ok(cur)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::entry::{compare, compare_raw, encode_into, encoded_len};
    use crate::header::Type;
    use std::vec::Vec;

    fn five_entry_block() -> ([u8; 64], BlockHeader) {
        let vals: [EntryVal; 5] = [
            EntryVal::Uint(7),
            EntryVal::Str(b"beta"),
            EntryVal::Uint(250),
            EntryVal::Str(b"alpha"),
            EntryVal::Uint(65536),
        ];
        let mut buf = [0u8; 64];
        let mut off = HEADER_SIZE;
        let mut last_off = HEADER_SIZE;
        for v in &vals {
            let len = encoded_len(v);
            encode_into(v, &mut buf[off..off + len]);
            last_off = off;
            off += len;
        }
        let hdr = BlockHeader {
            type_: Type::List,
            format: 0,
            flags: 0,
            nentry: vals.len() as u32,
            tail_off: last_off as u32,
        };
        hdr.write_to(&mut buf[..HEADER_SIZE]);
        (buf, hdr)
    }

    fn iter_fwd<'a>(buf: &'a [u8], hdr: &'a BlockHeader) -> impl Iterator<Item = Cursor> + 'a {
        core::iter::successors(Cursor::first(buf, hdr), move |c| c.next(buf))
    }

    fn iter_bwd<'a>(buf: &'a [u8], hdr: &'a BlockHeader) -> impl Iterator<Item = Cursor> + 'a {
        core::iter::successors(Cursor::last(buf, hdr), move |c| c.prev(buf, hdr))
    }

    #[test]
    fn forward_and_backward_walks_agree() {
        let (buf, hdr) = five_entry_block();
        let fwd: Vec<usize> = iter_fwd(&buf, &hdr).map(|c| c.off).collect();
        let mut bwd: Vec<usize> = iter_bwd(&buf, &hdr).map(|c| c.off).collect();
        bwd.reverse();
        assert_eq!(fwd, bwd);
        assert_eq!(fwd.len(), 5);
        assert_eq!(fwd[0], HEADER_SIZE);
        assert_eq!(*fwd.last().unwrap(), hdr.tail_off as usize);
    }

    #[test]
    fn locate_matches_walk_from_both_ends() {
        let (buf, hdr) = five_entry_block();
        for i in 0..5u32 {
            let by_walk = iter_fwd(&buf, &hdr).nth(i as usize).unwrap();
            assert_eq!(locate(&buf, &hdr, i).unwrap().off, by_walk.off, "i={i}");
        }
        assert!(locate(&buf, &hdr, 5).is_err());
    }

    #[test]
    fn comparator_total_order() {
        use core::cmp::Ordering::*;
        assert_eq!(compare(&EntryVal::Uint(9), &EntryVal::Str(b"1")), Less); // int < str
        assert_eq!(compare(&EntryVal::Uint(2), &EntryVal::Uint(10)), Less); // numeric
        assert_eq!(compare(&EntryVal::Str(b"10"), &EntryVal::Str(b"9")), Less); // lex
                                                                                // compare_raw classifies canonical decimals as ints:
        assert_eq!(compare_raw(b"2", b"10"), Less);
        assert_eq!(compare_raw(b"01", b"1"), Greater); // "01" is a string
    }
}
