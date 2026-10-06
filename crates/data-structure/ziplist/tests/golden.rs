//! Golden byte-freeze tests: build a small, fixed collection of each type
//! and assert the *exact* encoded bytes, derived by hand from the format
//! spec (see `crates/ziplist/src/header.rs` and `crates/ziplist/src/entry.rs`
//! doc comments). Any future diff here is a `(type, 0x00)` format break.
//!
//! # Entry encoding recap
//!
//! Each entry is `tag [data] backlen`:
//! - tag `0..=250`: immediate unsigned value, no data bytes. `tag_plus_data`
//!   (the span `encode_backlen` covers) is `1`.
//! - tag `255`: a string. Payload is a forward varint length (single byte
//!   for lengths `< 128`) followed by that many data bytes.
//! - `backlen`: a *backward*-read varint encoding `tag_plus_data`. For any
//!   value `< 128` (true for every entry in these small fixtures) it's a
//!   single byte equal to that value, with bit7 clear.
//!
//! # Header recap (12 bytes, all multi-byte fields little-endian)
//!
//! `type: u8, format: u8, flags: u16, nentry: u32, tail_off: u32`.
//! `tail_off` is the *start offset* of the last entry (or the *value*
//! entry of the last pair, for hash/zset).

use ziplist::{
    decode, encode_into, encoded_len, Block, EntryVal, HashMut, ListMut, SetMut, ZsetMut,
};

/// Parses `buf` as a generic block (no type-specific validation) and
/// returns its exact used-length byte slice -- header plus entries, through
/// the end of the last entry -- which is what every `*Mut::init`-built
/// buffer's leading bytes must match byte-for-byte.
fn used_bytes(buf: &[u8]) -> &[u8] {
    Block::parse(buf).unwrap().bytes()
}

#[test]
fn list_golden_bytes() {
    // push_back(Uint(1)), push_back(Str(b"ab")).
    //
    // Entry 1, Uint(1): tag = 1 (immediate, v <= 250), no data.
    //   tag_plus_data = 1, backlen = [0x01]. Bytes: 01 01 (2 bytes).
    // Entry 2, Str(b"ab"): tag = 255 (0xFF), varint length = 1 byte (0x02),
    //   data = "ab" = 61 62. tag_plus_data = 1 + 1 + 2 = 4,
    //   backlen = [0x04]. Bytes: FF 02 61 62 04 (5 bytes).
    //
    // Header: type=List(0), format=0, flags=0, nentry=2,
    //   tail_off = 12 (header) + 2 (entry 1) = 14 = 0x0E.
    let mut buf = vec![0u8; 256];
    let mut l = ListMut::init(&mut buf).unwrap();
    l.push_back(&EntryVal::Uint(1)).unwrap();
    l.push_back(&EntryVal::Str(b"ab")).unwrap();

    #[rustfmt::skip]
    let expected: [u8; 19] = [
        0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, // header
        0x01, 0x01, // Uint(1)
        0xFF, 0x02, 0x61, 0x62, 0x04, // Str("ab")
    ];
    assert_eq!(used_bytes(&buf), &expected[..]);
}

#[test]
fn hash_golden_bytes() {
    // hset(b"5", b"9"), hset(b"z", b"ab").
    //
    // Field "5" canonicalizes to Uint(5); field "z" is not numeric, stays
    // Str. Hash body sorts by field: Uint before Str, so the (field, value)
    // pairs land in insertion order here: (5, "9"), ("z", "ab"). Values are
    // always stored as Str, verbatim, regardless of how they look.
    //
    // Entry 1, field Uint(5): tag = 5, no data. tag_plus_data = 1,
    //   backlen = [0x01]. Bytes: 05 01 (2 bytes).
    // Entry 2, value Str(b"9"): tag = 255, varint length = 1 byte (0x01),
    //   data = "9" = 39. tag_plus_data = 1 + 1 + 1 = 3, backlen = [0x03].
    //   Bytes: FF 01 39 03 (4 bytes).
    // Entry 3, field Str(b"z"): tag = 255, varint length = 1 byte (0x01),
    //   data = "z" = 7A. tag_plus_data = 3, backlen = [0x03].
    //   Bytes: FF 01 7A 03 (4 bytes).
    // Entry 4, value Str(b"ab"): tag = 255, varint length = 1 byte (0x02),
    //   data = "ab" = 61 62. tag_plus_data = 1 + 1 + 2 = 4, backlen = [0x04].
    //   Bytes: FF 02 61 62 04 (5 bytes).
    //
    // Header: type=Hash(1), format=0, flags=0, nentry=4,
    //   tail_off = 12 + 2 (entry 1) + 4 (entry 2) + 4 (entry 3) = 22 = 0x16
    //   (start of entry 4, the last pair's value entry).
    let mut buf = vec![0u8; 256];
    let mut h = HashMut::init(&mut buf).unwrap();
    h.hset(b"5", b"9").unwrap();
    h.hset(b"z", b"ab").unwrap();

    #[rustfmt::skip]
    let expected: [u8; 27] = [
        0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x16, 0x00, 0x00, 0x00, // header
        0x05, 0x01, // field Uint(5)
        0xFF, 0x01, 0x39, 0x03, // value Str("9")
        0xFF, 0x01, 0x7A, 0x03, // field Str("z")
        0xFF, 0x02, 0x61, 0x62, 0x04, // value Str("ab")
    ];
    assert_eq!(used_bytes(&buf), &expected[..]);
}

#[test]
fn set_golden_bytes() {
    // sadd(b"10"), sadd(b"a").
    //
    // "10" canonicalizes to Uint(10); "a" is not numeric, stays Str. Body
    // sorts Uint before Str, matching insertion order here.
    //
    // Entry 1, Uint(10): tag = 10 (0x0A), no data. tag_plus_data = 1,
    //   backlen = [0x01]. Bytes: 0A 01 (2 bytes).
    // Entry 2, Str(b"a"): tag = 255, varint length = 1 byte (0x01),
    //   data = "a" = 61. tag_plus_data = 1 + 1 + 1 = 3, backlen = [0x03].
    //   Bytes: FF 01 61 03 (4 bytes).
    //
    // Header: type=Set(2), format=0, flags=0, nentry=2,
    //   tail_off = 12 + 2 (entry 1) = 14 = 0x0E.
    let mut buf = vec![0u8; 256];
    let mut s = SetMut::init(&mut buf).unwrap();
    s.sadd(b"10").unwrap();
    s.sadd(b"a").unwrap();

    #[rustfmt::skip]
    let expected: [u8; 18] = [
        0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, // header
        0x0A, 0x01, // Uint(10)
        0xFF, 0x01, 0x61, 0x03, // Str("a")
    ];
    assert_eq!(used_bytes(&buf), &expected[..]);
}

#[test]
fn zset_golden_bytes() {
    // zadd(b"a", 5), zadd(b"b", 3).
    //
    // Body sorts by (score asc, member tiebreak): score 3 ("b") before
    // score 5 ("a") -- the reverse of insertion order. Each pair is
    // (member, score) adjacent entries, member first.
    //
    // Entry 1, member Str(b"b"): tag = 255, varint length = 1 byte (0x01),
    //   data = "b" = 62. tag_plus_data = 1 + 1 + 1 = 3, backlen = [0x03].
    //   Bytes: FF 01 62 03 (4 bytes).
    // Entry 2, score Uint(3): tag = 3, no data. tag_plus_data = 1,
    //   backlen = [0x01]. Bytes: 03 01 (2 bytes).
    // Entry 3, member Str(b"a"): tag = 255, varint length = 1 byte (0x01),
    //   data = "a" = 61. tag_plus_data = 3, backlen = [0x03].
    //   Bytes: FF 01 61 03 (4 bytes).
    // Entry 4, score Uint(5): tag = 5, no data. tag_plus_data = 1,
    //   backlen = [0x01]. Bytes: 05 01 (2 bytes).
    //
    // Header: type=Zset(3), format=0, flags=0, nentry=4,
    //   tail_off = 12 + 4 (entry 1) + 2 (entry 2) + 4 (entry 3) = 22 = 0x16
    //   (start of entry 4, the last pair's score entry).
    let mut buf = vec![0u8; 256];
    let mut z = ZsetMut::init(&mut buf).unwrap();
    z.zadd(b"a", 5).unwrap();
    z.zadd(b"b", 3).unwrap();

    #[rustfmt::skip]
    let expected: [u8; 24] = [
        0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x16, 0x00, 0x00, 0x00, // header
        0xFF, 0x01, 0x62, 0x03, // member Str("b")
        0x03, 0x01, // score Uint(3)
        0xFF, 0x01, 0x61, 0x03, // member Str("a")
        0x05, 0x01, // score Uint(5)
    ];
    assert_eq!(used_bytes(&buf), &expected[..]);
}

/// Pins the exact entry-codec bytes for each multi-byte uint tag tier
/// (251/252/253/254), one value each, hand-derived. Existing fixtures above
/// only ever use immediate tags (`0..=250`) with 1-byte backlens, so an
/// endianness flip or a tier-boundary regression in `tag_and_data_len`
/// would pass every other committed test. Boundary values chosen to match
/// `entry.rs`'s `uint_encoded_sizes` unit test (same values, sizes only).
#[test]
fn uint_tier_golden_bytes() {
    // tag 251 (u16 tier): v = 65535 = u16::MAX = 0xFFFF.
    //   tag = 251 = 0xFB, data = LE(0xFFFF) = FF FF (2 bytes).
    //   tag_plus_data = 1 + 2 = 3, backlen = [0x03] (< 128, 1 byte).
    //   Bytes: FB FF FF 03 (4 bytes).
    check_uint_golden(65535, &[0xFB, 0xFF, 0xFF, 0x03]);

    // tag 252 (u24 tier): v = 16777215 = 2^24 - 1 = 0xFFFFFF.
    //   tag = 252 = 0xFC, data = LE(0x00FFFFFF)[..3] = FF FF FF (3 bytes).
    //   tag_plus_data = 1 + 3 = 4, backlen = [0x04] (< 128, 1 byte).
    //   Bytes: FC FF FF FF 04 (5 bytes).
    check_uint_golden(16_777_215, &[0xFC, 0xFF, 0xFF, 0xFF, 0x04]);

    // tag 253 (u56 tier): v = 2^56 - 1 = 0x00FFFFFFFFFFFFFF.
    //   tag = 253 = 0xFD, data = LE(v)[..7] = FF FF FF FF FF FF FF (7 bytes,
    //   the excluded 8th LE byte is the leading 0x00).
    //   tag_plus_data = 1 + 7 = 8, backlen = [0x08] (< 128, 1 byte).
    //   Bytes: FD FF FF FF FF FF FF FF 08 (9 bytes).
    check_uint_golden(
        (1u64 << 56) - 1,
        &[0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x08],
    );

    // tag 254 (u64 tier): v = u64::MAX = 0xFFFFFFFFFFFFFFFF.
    //   tag = 254 = 0xFE, data = LE(v) = FF * 8 (8 bytes).
    //   tag_plus_data = 1 + 8 = 9, backlen = [0x09] (< 128, 1 byte).
    //   Bytes: FE FF FF FF FF FF FF FF FF 09 (10 bytes).
    check_uint_golden(
        u64::MAX,
        &[0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x09],
    );
}

/// Encodes `v` via `encode_into`, asserts the exact byte sequence, then
/// decodes those bytes back (forward and via `decode_backward`) and asserts
/// the roundtrip.
fn check_uint_golden(v: u64, expected: &[u8]) {
    let val = EntryVal::Uint(v);
    let n = encoded_len(&val);
    assert_eq!(n, expected.len(), "encoded_len mismatch for v={v}");
    let mut buf = [0u8; 16];
    assert_eq!(encode_into(&val, &mut buf[..n]), n);
    assert_eq!(&buf[..n], expected, "encoded bytes mismatch for v={v}");

    let (got, len) = decode(&buf[..n], 0).unwrap();
    assert_eq!(len, n);
    assert_eq!(got, val);
    assert_eq!(ziplist::decode_backward(&buf[..n], n).unwrap(), 0);
}

/// Pins the exact entry-codec bytes for a string long enough to force a
/// 2-byte forward length varint *and* a 2-byte backward backlen -- both
/// only ever exercised via 1-byte encodings in the fixtures above. Both
/// varints are derived by hand.
#[test]
fn long_string_golden_bytes() {
    // Str of 128 'x' bytes (0x78 each).
    //
    // Forward length varint for 128 (0b1000_0000): 7-bit groups, low group
    // first, bit7 = continue.
    //   group0 = 128 & 0x7F = 0, more remains (128 >> 7 = 1 != 0) -> 0x80.
    //   group1 = 1 & 0x7F = 1, nothing remains -> 0x01.
    //   Bytes: 80 01 (2 bytes).
    //
    // tag = 255 = 0xFF. tag_plus_data = 1 (tag) + 2 (varint) + 128 (data)
    //   = 131.
    //
    // Backward backlen for 131 (0b1000_0011): backward varint groups least
    // significant first while building, then written most-significant
    // (bit7 clear) byte first:
    //   group0 (LSB) = 131 & 0x7F = 3; 131 >> 7 = 1.
    //   group1 = 1 & 0x7F = 1; 1 >> 7 = 0 -> stop, 2 groups.
    //   Written leftmost (most significant) first, bit7 clear: byte0 =
    //   group1 = 0x01. Remaining byte, bit7 set: byte1 = group0 | 0x80 =
    //   0x03 | 0x80 = 0x83.
    //   Bytes: 01 83 (2 bytes). Decoding reads rightmost byte first:
    //   0x83 & 0x7F = 3 (shift 0), then 0x01 & 0x7F = 1 (shift 7) ->
    //   3 + (1 << 7) = 131. Matches.
    //
    // Full entry: FF 80 01 [128 * 0x78] 01 83 (133 bytes total).
    let data = [0x78u8; 128];
    let val = EntryVal::Str(&data);
    let n = encoded_len(&val);
    assert_eq!(n, 133, "encoded_len mismatch for 128-byte string");

    let mut expected = Vec::with_capacity(133);
    expected.push(0xFF);
    expected.extend_from_slice(&[0x80, 0x01]);
    expected.extend_from_slice(&data);
    expected.extend_from_slice(&[0x01, 0x83]);
    assert_eq!(expected.len(), 133);

    let mut buf = [0u8; 133];
    assert_eq!(encode_into(&val, &mut buf), 133);
    assert_eq!(&buf[..], &expected[..]);

    let (got, len) = decode(&buf, 0).unwrap();
    assert_eq!(len, 133);
    assert_eq!(got, val);
    assert_eq!(ziplist::decode_backward(&buf, 133).unwrap(), 0);
}
