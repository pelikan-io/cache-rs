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

use ziplist::{Block, EntryVal, HashMut, ListMut, SetMut, ZsetMut};

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
