//! Cursor-based traversal over ziplist entries.
//!
//! A [`Cursor`] points at a single entry within a block and can walk
//! forward or backward one entry at a time. [`locate`] jumps directly to
//! the `idx`-th entry by walking from whichever end of the block is
//! nearer, so index lookups cost at most `nentry / 2` steps.
//!
//! # `buf` MUST be sliced to the block's used length
//!
//! Every function here (`Cursor::first`, `Cursor::last`, `Cursor::next`,
//! `Cursor::prev`, `locate`) takes `buf: &[u8]` and trusts it to end
//! exactly at the block's used length: header plus entries, through the
//! last byte of the entry at `hdr.tail_off`. None of them are told the
//! backing buffer's full capacity, so none can distinguish "ran off the
//! end of the real entries" from "kept reading into leftover bytes beyond
//! the tail that happen to still decode as a plausible entry" (e.g. a
//! `BlockMut`'s backing storage after a shrinking splice leaves stale
//! bytes past the new tail). Passing an over-long `buf` — the full
//! capacity of a larger allocation instead of a slice trimmed to the used
//! length — can make `Cursor::next` (and therefore `locate`) walk past the
//! real tail and return a stale, garbage entry instead of `None`. This is
//! a correctness precondition, not a memory-safety one: out-of-bounds
//! reads are still impossible (`decode`/`decode_backward` are
//! `.get()`-based with checked arithmetic throughout), but the "one past
//! the tail is `None`" contract only holds when the caller slices `buf` to
//! the used length first.

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
    ///
    /// # Preconditions
    ///
    /// `buf` MUST be sliced to the block's *used* length — header plus
    /// entries, ending exactly at the end of the entry at `hdr.tail_off` —
    /// never the full capacity of a larger backing buffer. Bytes beyond
    /// the used length are not part of this walk's contract; see the
    /// [module docs](self) for why passing an over-long `buf` is unsafe
    /// for correctness (though not memory-unsafe).
    pub fn first(buf: &[u8], hdr: &BlockHeader) -> Option<Cursor> {
        if hdr.nentry == 0 {
            return None;
        }
        Self::at(buf, HEADER_SIZE)
    }

    /// Returns a cursor to the last entry in the block, or `None` if the
    /// block is empty.
    ///
    /// # Preconditions
    ///
    /// Same as [`Cursor::first`]: `buf` MUST be sliced to the block's used
    /// length, not the backing buffer's full capacity.
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
    /// is the last entry.
    ///
    /// # Preconditions
    ///
    /// `buf` MUST be sliced to the block's used length (through the end of
    /// the entry at `hdr.tail_off`), never a larger backing capacity. This
    /// method has no `hdr` parameter, so it cannot compare against
    /// `tail_off` itself: "no next entry" is detected only by `decode`
    /// failing at `off + len`, which happens when that offset runs past
    /// `buf`'s end, or lands on bytes that don't decode as a valid entry
    /// (e.g. zero padding). If `buf` extends past the block's real content
    /// and happens to contain bytes there that decode successfully (stale
    /// or planted entry bytes, as a `BlockMut`'s backing buffer may have
    /// after a shrinking op), `next` cannot tell the difference and will
    /// return that stale entry instead of `None`. Callers MUST slice `buf`
    /// to the block's used length before walking, to keep this
    /// method's "one past the tail" contract truthful.
    pub fn next(&self, buf: &[u8]) -> Option<Cursor> {
        let next_off = self.off.checked_add(self.len)?;
        Self::at(buf, next_off)
    }

    /// Returns a cursor to the entry preceding this one, or `None` if this
    /// is the first entry.
    ///
    /// # Preconditions
    ///
    /// `buf` MUST be sliced to the block's used length, same as
    /// [`Cursor::first`]/[`Cursor::next`]. Unlike `next`, `prev` does take
    /// `hdr` and defensively checks `self.off` against `hdr.tail_off`, but
    /// that only guards against a cursor positioned past the declared
    /// tail — it does not, and cannot, validate that `buf` itself was
    /// sliced correctly.
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
///
/// # Preconditions
///
/// `buf` MUST be sliced to the block's used length, same as
/// [`Cursor::first`]/[`Cursor::next`]/[`Cursor::prev`] — see the
/// [module docs](self). Because `locate` may walk forward via
/// `Cursor::next`, an over-long `buf` can make it return a cursor onto
/// stale bytes past the real tail instead of an error.
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
    fn next_none_past_tail_requires_buf_sliced_to_used_length() {
        // A single-entry block in an oversized buffer, with a second,
        // independently valid entry's bytes planted immediately after the
        // real tail -- simulating stale bytes left in a backing buffer's
        // spare capacity (e.g. after a shrinking BlockMut splice that
        // doesn't zero the vacated bytes).
        let val = EntryVal::Uint(42);
        let len = encoded_len(&val);
        let used_len = HEADER_SIZE + len;

        let mut buf = [0u8; 64];
        encode_into(&val, &mut buf[HEADER_SIZE..used_len]);
        let hdr = BlockHeader {
            type_: Type::List,
            format: 0,
            flags: 0,
            nentry: 1,
            tail_off: HEADER_SIZE as u32,
        };
        hdr.write_to(&mut buf[..HEADER_SIZE]);

        // Plant a second, independently-decodable entry right after the
        // real tail -- valid bytes, but not part of this block.
        let planted = EntryVal::Uint(99);
        let planted_len = encoded_len(&planted);
        encode_into(&planted, &mut buf[used_len..used_len + planted_len]);

        let tail = Cursor::last(&buf[..used_len], &hdr).unwrap();
        assert_eq!(tail.off, HEADER_SIZE);

        // Contract: buf sliced to the block's used length -> no next entry.
        assert_eq!(tail.next(&buf[..used_len]), None);

        // Pin the failure mode the contract guards against: an over-long
        // buf (here, the full oversized capacity) lets next() walk into
        // the planted bytes and return a bogus "next" entry instead.
        assert!(tail.next(&buf[..]).is_some());
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
