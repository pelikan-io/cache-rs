//! Entry codec: tags, varint backlen, and canonical integer rendering.
//!
//! Each entry is `tag [data] backlen`:
//! - tag `0..=250`: immediate unsigned value (no data bytes).
//! - tag `251/252/253/254`: little-endian payload of `2/3/7/8` bytes holding
//!   a `u16`/`u24`/`u56`/`u64` value (tag + data = `3/4/8/9` bytes).
//! - tag `255`: a forward varint string length (7-bit groups, bit7 =
//!   continue, max 5 bytes) followed by that many bytes of string data.
//! - `backlen`: a backward-read varint (7-bit groups, but read from the
//!   *end*; the leftmost byte has bit7 clear, all others have bit7 set)
//!   encoding the length of `tag + data`, letting decoders walk the list
//!   from the tail without a separate offset index.

use crate::error::DecodeError;
use core::cmp::Ordering;

/// A decoded entry value: either an unsigned integer or a string of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryVal<'a> {
    /// An unsigned 64-bit integer value.
    Uint(u64),
    /// A string of raw bytes.
    Str(&'a [u8]),
}

/// Maximum length of a rendered varint length prefix (5 groups of 7 bits).
const MAX_VARINT_LEN: usize = 5;

/// Length (in bytes) of the payload for each integer tag tier, excluding
/// the tag byte itself.
const U16_BYTES: usize = 2;
const U24_BYTES: usize = 3;
const U56_BYTES: usize = 7;
const U64_BYTES: usize = 8;

const TAG_U16: u8 = 251;
const TAG_U24: u8 = 252;
const TAG_U56: u8 = 253;
const TAG_U64: u8 = 254;
const TAG_STR: u8 = 255;

/// Returns `Some(v)` iff `bytes` is the canonical decimal rendering of `v`:
/// nonempty, digits only, no leading zero unless exactly `"0"`, and fits in
/// a `u64`.
pub fn canonical_uint(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() > 1 {
        return None;
    }
    let mut val: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        let digit = (b - b'0') as u64;
        val = val.checked_mul(10)?.checked_add(digit)?;
    }
    Some(val)
}

/// Renders `v` as canonical decimal bytes into `out`, returning the used
/// slice (no leading zeros, except for the value `0` itself).
pub fn render_uint(v: u64, out: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        out[0] = b'0';
        return &out[..1];
    }
    let mut tmp = [0u8; 20];
    let mut i = 20;
    let mut v = v;
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let len = 20 - i;
    out[..len].copy_from_slice(&tmp[i..]);
    &out[..len]
}

/// Returns the tag byte and number of payload data bytes (excluding tag and
/// backlen) needed to encode `val`.
fn tag_and_data_len(val: &EntryVal) -> (u8, usize) {
    match *val {
        EntryVal::Uint(v) => {
            if v <= 250 {
                (v as u8, 0)
            } else if v <= u16::MAX as u64 {
                (TAG_U16, U16_BYTES)
            } else if v < (1 << 24) {
                (TAG_U24, U24_BYTES)
            } else if v < (1 << 56) {
                (TAG_U56, U56_BYTES)
            } else {
                (TAG_U64, U64_BYTES)
            }
        }
        EntryVal::Str(s) => (TAG_STR, varint_len(s.len()) + s.len()),
    }
}

/// Number of bytes needed to encode `len` as a forward varint (7-bit
/// groups, bit7 = continue).
fn varint_len(len: usize) -> usize {
    let mut n = 1;
    let mut v = len >> 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

/// Writes `len` as a forward varint (7-bit little-endian groups, bit7 set
/// on all but the last byte) into `out`, returning the number of bytes
/// written.
fn encode_varint_len(len: usize, out: &mut [u8]) -> usize {
    let mut v = len;
    let mut i = 0;
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out[i] = b;
        i += 1;
        if v == 0 {
            break;
        }
    }
    i
}

/// Reads a forward varint string length starting at `buf[off]`. Returns the
/// decoded length and number of bytes consumed.
fn decode_varint_len(buf: &[u8], off: usize) -> Result<(usize, usize), DecodeError> {
    let mut val: usize = 0;
    let mut shift: u32 = 0;
    let mut used: usize = 0;
    loop {
        let idx = off.checked_add(used).ok_or(DecodeError::Corrupt)?;
        let b = *buf.get(idx).ok_or(DecodeError::Truncated)?;
        let group = (b & 0x7F) as usize;
        let shifted = group.checked_shl(shift).ok_or(DecodeError::Corrupt)?;
        val = val.checked_add(shifted).ok_or(DecodeError::Corrupt)?;
        used += 1;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if used == MAX_VARINT_LEN {
            return Err(DecodeError::Corrupt);
        }
    }
    Ok((val, used))
}

/// Encodes the backward-readable varint length prefix for an entry's
/// `tag + data` span. `len` must be nonzero and fit in 35 bits (5 groups).
/// The rightmost (last) written byte is least significant; every byte
/// except the leftmost (first written) has bit7 set. Returns the number of
/// bytes written.
pub(crate) fn encode_backlen(len: usize, out: &mut [u8]) -> usize {
    debug_assert!(len > 0 && len <= 0x0000_FFFF_FFFF); // 35 bits max (5 groups)
    let mut groups = [0u8; 5];
    let mut n = 0;
    let mut v = len;
    loop {
        groups[n] = (v & 0x7F) as u8; // groups[0] = least significant
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    // write leftmost (most significant, bit7 clear) first, then the rest
    // with bit7 set, ending at the rightmost (least significant) byte.
    for i in 0..n {
        let g = groups[n - 1 - i];
        out[i] = if i == 0 { g } else { g | 0x80 };
    }
    n
}

/// Decodes the backward-readable varint length prefix ending just before
/// `end` (i.e. the entry's backlen occupies `buf[..end]`'s trailing bytes).
/// Returns `(tag_plus_data_len, backlen_bytes)`.
pub(crate) fn decode_backlen(buf: &[u8], end: usize) -> Result<(usize, usize), DecodeError> {
    let mut val: usize = 0;
    let mut shift = 0u32;
    let mut used = 0usize;
    loop {
        let idx = end.checked_sub(1 + used).ok_or(DecodeError::Corrupt)?;
        let b = *buf.get(idx).ok_or(DecodeError::Truncated)?;
        val |= ((b & 0x7F) as usize) << shift;
        used += 1;
        if b & 0x80 == 0 {
            break; // leftmost byte reached
        }
        shift += 7;
        if used == 5 {
            return Err(DecodeError::Corrupt);
        }
    }
    if val == 0 {
        return Err(DecodeError::Corrupt);
    }
    Ok((val, used))
}

/// Total encoded length (tag + data + backlen) needed for `val`.
pub fn encoded_len(val: &EntryVal) -> usize {
    let (_, data_len) = tag_and_data_len(val);
    let tag_plus_data = 1 + data_len;
    tag_plus_data + varint_len_for_backlen(tag_plus_data)
}

/// Number of bytes `encode_backlen` will emit for `len`.
fn varint_len_for_backlen(len: usize) -> usize {
    let mut n = 1;
    let mut v = len >> 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

/// Encodes `val` (tag + data + backlen) into `buf`, returning the number of
/// bytes written. The caller must ensure `buf.len() >= encoded_len(val)`
/// (typically by calling `encoded_len` first).
pub fn encode_into(val: &EntryVal, buf: &mut [u8]) -> usize {
    let (tag, data_len) = tag_and_data_len(val);
    buf[0] = tag;
    let mut pos = 1;
    match *val {
        EntryVal::Uint(v) => {
            let le = v.to_le_bytes();
            buf[pos..pos + data_len].copy_from_slice(&le[..data_len]);
            pos += data_len;
        }
        EntryVal::Str(s) => {
            let n = encode_varint_len(s.len(), &mut buf[pos..]);
            pos += n;
            buf[pos..pos + s.len()].copy_from_slice(s);
            pos += s.len();
        }
    }
    let backlen_written = encode_backlen(pos, &mut buf[pos..]);
    pos + backlen_written
}

/// Decodes the entry starting at `buf[off]`. Returns the decoded value and
/// the total length (tag + data + backlen) of the entry on success.
pub fn decode(buf: &[u8], off: usize) -> Result<(EntryVal<'_>, usize), DecodeError> {
    let tag = *buf.get(off).ok_or(DecodeError::Truncated)?;
    let data_off = off.checked_add(1).ok_or(DecodeError::Corrupt)?;

    let (val, tag_plus_data): (EntryVal<'_>, usize) = if tag <= 250 {
        (EntryVal::Uint(tag as u64), 1)
    } else if tag == TAG_STR {
        let (str_len, varint_bytes) = decode_varint_len(buf, data_off)?;
        let str_off = data_off
            .checked_add(varint_bytes)
            .ok_or(DecodeError::Corrupt)?;
        let str_end = str_off.checked_add(str_len).ok_or(DecodeError::Corrupt)?;
        let s = buf.get(str_off..str_end).ok_or(DecodeError::Truncated)?;
        let tag_plus_data = 1usize
            .checked_add(varint_bytes)
            .and_then(|n| n.checked_add(str_len))
            .ok_or(DecodeError::Corrupt)?;
        (EntryVal::Str(s), tag_plus_data)
    } else {
        let data_len = match tag {
            TAG_U16 => U16_BYTES,
            TAG_U24 => U24_BYTES,
            TAG_U56 => U56_BYTES,
            TAG_U64 => U64_BYTES,
            other => return Err(DecodeError::UnknownFormat(other)),
        };
        let data_end = data_off.checked_add(data_len).ok_or(DecodeError::Corrupt)?;
        let bytes = buf.get(data_off..data_end).ok_or(DecodeError::Truncated)?;
        let mut le = [0u8; 8];
        le[..data_len].copy_from_slice(bytes);
        let v = u64::from_le_bytes(le);
        (EntryVal::Uint(v), 1 + data_len)
    };

    // The backlen encodes `tag_plus_data`, which we already know from
    // decoding the tag and data above; forward-reading a backward-oriented
    // varint has no in-band terminator, so instead of re-parsing it we
    // recompute its expected bytes and compare against the buffer. This
    // doubles as the cheap corruption tripwire the format is designed for.
    let backlen_bytes = varint_len_for_backlen(tag_plus_data);
    let backlen_off = off.checked_add(tag_plus_data).ok_or(DecodeError::Corrupt)?;
    let backlen_end = backlen_off
        .checked_add(backlen_bytes)
        .ok_or(DecodeError::Corrupt)?;
    let found = buf
        .get(backlen_off..backlen_end)
        .ok_or(DecodeError::Truncated)?;
    let mut expected = [0u8; MAX_VARINT_LEN];
    let expected_len = encode_backlen(tag_plus_data, &mut expected);
    if found != &expected[..expected_len] {
        return Err(DecodeError::Corrupt);
    }

    let total = tag_plus_data
        .checked_add(backlen_bytes)
        .ok_or(DecodeError::Corrupt)?;
    Ok((val, total))
}

/// Given the offset one past an entry's last byte, returns the entry's
/// start offset.
pub fn decode_backward(buf: &[u8], end: usize) -> Result<usize, DecodeError> {
    let (tag_plus_data, backlen_bytes) = decode_backlen(buf, end)?;
    let after_backlen = end.checked_sub(backlen_bytes).ok_or(DecodeError::Corrupt)?;
    after_backlen
        .checked_sub(tag_plus_data)
        .ok_or(DecodeError::Corrupt)
}

/// Total order over entry values: `Uint` always sorts before `Str`; two
/// `Uint`s compare by value; two `Str`s compare byte-lexicographically.
pub fn compare(a: &EntryVal<'_>, b: &EntryVal<'_>) -> Ordering {
    match (a, b) {
        (EntryVal::Uint(x), EntryVal::Uint(y)) => x.cmp(y),
        (EntryVal::Str(x), EntryVal::Str(y)) => x.cmp(y),
        (EntryVal::Uint(_), EntryVal::Str(_)) => Ordering::Less,
        (EntryVal::Str(_), EntryVal::Uint(_)) => Ordering::Greater,
    }
}

/// Convenience wrapper over [`compare`] for raw client bytes: each side is
/// classified via [`canonical_uint`] first (so callers can pass client
/// bytes directly), so a canonical decimal rendering compares as `Uint`
/// and anything else compares as `Str`.
pub fn compare_raw(a: &[u8], b: &[u8]) -> Ordering {
    let av = canonical_uint(a).map_or(EntryVal::Str(a), EntryVal::Uint);
    let bv = canonical_uint(b).map_or(EntryVal::Str(b), EntryVal::Uint);
    compare(&av, &bv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(val: EntryVal) {
        // Sized to fit the largest value under test (20_000-byte strings),
        // plus tag/varint-length/backlen overhead.
        let mut buf = [0u8; 20_020];
        let n = encoded_len(&val);
        assert_eq!(encode_into(&val, &mut buf[..n]), n);
        let (got, len) = decode(&buf[..n], 0).unwrap();
        assert_eq!(len, n);
        assert_eq!(got, val);
        assert_eq!(decode_backward(&buf[..n], n).unwrap(), 0);
    }

    #[test]
    fn uint_tiers_roundtrip_at_boundaries() {
        for v in [
            0,
            1,
            249,
            250,
            251,
            255,
            256,
            u16::MAX as u64,
            u16::MAX as u64 + 1,
            (1 << 24) - 1,
            1 << 24,
            (1 << 56) - 1,
            1 << 56,
            u64::MAX,
        ] {
            roundtrip(EntryVal::Uint(v));
        }
    }

    #[test]
    fn uint_encoded_sizes() {
        // tag-only + 1-byte backlen = 2; then 3+1, 4+1, 8+1, 9+1
        for (v, sz) in [
            (0u64, 2),
            (250, 2),
            (251, 4),
            (65535, 4),
            (65536, 5),
            ((1 << 24) - 1, 5),
            (1 << 24, 9),
            ((1 << 56) - 1, 9),
            (1 << 56, 10),
            (u64::MAX, 10),
        ] {
            assert_eq!(encoded_len(&EntryVal::Uint(v)), sz, "v={v}");
        }
    }

    #[test]
    fn strings_roundtrip_including_large() {
        for len in [0usize, 1, 126, 127, 128, 251, 252, 253, 300, 20_000] {
            let data = [b'x'; 20_000];
            roundtrip(EntryVal::Str(&data[..len]));
        }
    }

    #[test]
    fn canonical_uint_rules() {
        assert_eq!(canonical_uint(b"0"), Some(0));
        assert_eq!(canonical_uint(b"42"), Some(42));
        assert_eq!(canonical_uint(b"18446744073709551615"), Some(u64::MAX));
        for bad in [
            &b""[..],
            b"01",
            b"-1",
            b"+1",
            b"1.5",
            b"1e3",
            b"18446744073709551616",
            b"00",
        ] {
            assert_eq!(canonical_uint(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn render_uint_is_canonical_inverse() {
        let mut out = [0u8; 20];
        assert_eq!(render_uint(0, &mut out), b"0");
        assert_eq!(render_uint(u64::MAX, &mut out), b"18446744073709551615");
        assert_eq!(
            canonical_uint(render_uint(9876543210, &mut out)),
            Some(9876543210)
        );
    }

    #[test]
    fn backlen_golden_vectors() {
        // (value, encoded bytes leftmost..rightmost)
        for (v, bytes) in [
            (1usize, &[0x01u8][..]),
            (127, &[0x7F]),
            (128, &[0x01, 0x80]),
            (16383, &[0x7F, 0xFF]),
            (16384, &[0x01, 0x80, 0x80]),
        ] {
            let mut buf = [0u8; 5];
            assert_eq!(encode_backlen(v, &mut buf), bytes.len());
            assert_eq!(&buf[..bytes.len()], bytes, "v={v}");
        }
    }
}
