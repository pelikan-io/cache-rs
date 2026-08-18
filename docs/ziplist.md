# ziplist: Compact Byte-Format Collections Codec

## Overview

`ziplist` encodes lists, hashes, sets, and sorted sets ("zsets") as a single
contiguous byte block, in the spirit of Redis's listpack/ziplist. There is no
allocation inside the crate (`#![no_std]`, `#![forbid(unsafe_code)]`): every
operation is a pure function over a caller-supplied `&[u8]` / `&mut [u8]`, so
the crate has no opinion on where that buffer comes from -- a segcache item's
value bytes, an mmap'd region, a stack array in a test.

A block is self-describing: a fixed 12-byte header carries the type, an
entry count, and the offset of the last entry, so cardinality and tail
access are O(1) without scanning. Everything after the header is a flat
sequence of *entries* -- immediate integers or length-prefixed strings, each
carrying its own backward-readable length so the block can be walked in
either direction without a separate index.

Four typed views sit on top of the raw block, each pairing a read-only
`*View` with a mutable `*Mut`, mirroring a slice of the Redis command set:

| Type | Views | Ops | Body convention |
|---|---|---|---|
| List | `ListView` / `ListMut` | `push_front`/`push_back`/`pop_front`/`pop_back`/`trim`/`index`/`range` | one entry per element, positional |
| Hash | `HashView` / `HashMut` | `hset`/`hget`/`hdel`/`hincrby`/`iter_pairs` | `(field, value)` pairs, sorted by field |
| Set | `SetView` / `SetMut` | `sadd`/`srem`/`sismember`/`iter_members` | one entry per member, sorted |
| Zset | `ZsetView` / `ZsetMut` | `zadd`/`zrem`/`zincrby`/`zscore`/`zrange_by_score`/`zrange_by_rank` | `(member, score)` pairs, sorted by `(score, member)`, `u64` scores only |

## Block Header

12 bytes, all multi-byte fields little-endian:

| offset | size | field | meaning |
|---|---|---|---|
| 0 | 1 | `type` | logical type: `0`=List `1`=Hash `2`=Set `3`=Zset |
| 1 | 1 | `format` | owned by `type`; `0x00` is that type's canonical v1 layout |
| 2 | 2 | `flags` | orthogonal runtime bits; bit 0 = chain-root (reserved for a future directory format, unused by any `format = 0x00` body); other bits reserved, must be zero |
| 4 | 4 | `nentry` | number of entries (`u32`) -- O(1) cardinality |
| 8 | 4 | `tail_off` | offset of the last entry's first byte (`u32`) -- O(1) tail access |
| 12 | ... | body | entries, back to back |

An empty body has `nentry = 0`, `tail_off = 12` (`HEADER_SIZE`).

`Block::parse`/`BlockMut::parse` validate the header and then walk every
entry from `HEADER_SIZE` to `tail_off`, confirming exactly `nentry` entries
are found and the one at `tail_off` decodes cleanly; the walk's endpoint
becomes the block's *used length*. A buffer may be longer than the used
length (a `BlockMut`'s backing storage is the block's *capacity*, not its
current size) -- `parse` only validates and measures the used prefix.

## Entry Codec

Uniform across every `format = 0x00` layout. An entry is `tag [data]
backlen`:

| tag | data | value | entry size (excl. backlen) |
|---|---|---|---|
| `0..=250` | none | the tag itself | 1 |
| `251` | `u16` LE | uint <= 2^16-1 | 3 |
| `252` | `u24` LE | uint <= 2^24-1 | 4 |
| `253` | `u56` LE | uint <= 2^56-1 | 8 |
| `254` | `u64` LE | uint <= 2^64-1 | 9 |
| `255` | varint len, then that many bytes | string | `1 + varint_len + len` |

**Canonical integers.** A value is stored as an integer only if its bytes
are the canonical decimal rendering of a `u64`: no sign, no leading zero
(except the literal string `"0"`), and it fits in 64 bits. `canonical_uint`
implements the check; `render_uint` is its inverse. Everything else is
stored as a string. This is what makes `HSET k 42 v` and `HGET k 42` agree,
and why an integer field/member always sorts before every string one.

**Comparator.** [`compare`] defines the crate's one total order, used for
every sorted body: integers sort before strings; two integers compare by
value; two strings compare byte-lexicographically. [`compare_raw`] is a
convenience wrapper that classifies raw client bytes via `canonical_uint`
first, so callers can compare raw bytes directly without decoding.

### Backlen Encoding

`backlen` is a variable-length integer encoding the length of the entry's
own `tag + data` span (not including the backlen itself), written so it can
be read *backward* from the entry's end -- this is what lets a block be
walked tail-to-head without a separate offset index, and it never cascades:
re-encoding one entry never changes any other entry's bytes.

It uses 7-bit groups, most-significant group first:

- The **leftmost** (first, lowest-address) byte has bit 7 **clear**.
- Every other byte has bit 7 **set** (a "more bytes to the left" marker,
  read when walking backward from the right).
- Values up to 127 fit in a single byte; the encoding uses at most 5 bytes
  (35 bits), which is far more than any real entry needs (the largest
  possible `tag + data` span, a maximal string entry's 5-byte varint length
  plus its data, is representable but any entry that huge would already be
  well past the addressable range of a single ziplist block).

| value | encoded bytes (leftmost..rightmost) |
|---|---|
| 1 | `01` |
| 127 | `7F` |
| 128 | `01 80` |
| 16383 | `7F FF` |
| 16384 | `01 80 80` |

To decode backward from a known end offset: read the byte immediately
before `end`. If bit 7 is set, it's the least-significant group and there
are more bytes to the left; keep stepping left, accumulating 7-bit groups
(least significant first, since we're reading right-to-left), until a byte
with bit 7 clear is found -- that's the most significant group, and the
walk is done. `decode_backward` implements this to recover an entry's start
offset from the offset one past its end.

## Per-Type Conventions (`format = 0x00`)

| type | body | ordering | notes |
|---|---|---|---|
| List | one entry per element | positional | `push_back` is O(1) at `tail_off`; `push_front` memmoves the body |
| Hash | `(field, value)` pairs | sorted by field | pair boundaries are implicit: even body-index = field, odd = value. Only the field is classified via `canonical_uint`; the value is always stored as `Str`, verbatim (a value like `b"9"` reads back as `EntryVal::Str(b"9")`, not `Uint(9)`) |
| Set | one entry per member | sorted by member | thin wrapper over the same seek machinery hash uses, with no paired value |
| Zset | `(member, score)` pairs | sorted by `(score, member)` | member first, then score; score is always a canonical `u64` integer entry. Member lookups (`zscore`/`zadd`/`zrem`/`zincrby`) are a linear scan -- the body is score-sorted, not member-sorted, so they can't binary-search the way a hash's field lookup does |

Keyed lookups are linear scans of the block (hash/set use a sorted-body seek
that can stop early; zset's member lookups scan every pair). This is
intentional: block size is bounded by the caller (e.g. an item size cap),
so `O(nentry)` at block scale is cheap in practice -- see `benches/codec.rs`
for the `nentry` sweep this trades off.

Every `(type, format)` pair is a **totally specified layout**. An unknown
`type` or `format` byte is a decode error (`DecodeError::UnknownType`/
`UnknownFormat`), never a fallback guess -- decoders dispatch on the pair,
and the internal structure of `format` is invisible outside that type's
module. This is what lets a future format (e.g. a fixed-stride `List/0x01`
for uniform-integer lists, or a v2 chained/directory body behind the
chain-root flag) be added later without touching any other type's decoder,
and it's why every fuzz/proptest/kani harness in this crate treats "unknown
or malformed bytes" as "must return `Err`, never panic or guess" rather
than "must decode somehow."

## Worked Example: A Hash Block

Building a 2-field hash step by step, reproducing
`tests/golden.rs::hash_golden_bytes` byte for byte:

```rust
let mut h = HashMut::init(&mut buf)?;
h.hset(b"5", b"9")?;
h.hset(b"z", b"ab")?;
```

Field `"5"` canonicalizes to `Uint(5)`; field `"z"` is not numeric, so it
stays `Str`. A hash body sorts by field, and integers sort before strings,
so the two `(field, value)` pairs land in insertion order here: `(5, "9")`,
then `("z", "ab")`. Values are always stored verbatim as `Str`, regardless
of what they look like.

**Entry 1 -- field `Uint(5)`:** tag = `5` (an immediate value, `<= 250`), no
data bytes. `tag_plus_data = 1`, so `backlen = [0x01]`.
Bytes: `05 01` (2 bytes).

**Entry 2 -- value `Str(b"9")`:** tag = `255` (`0xFF`), varint length = 1
byte (`0x01`, since `1 < 128`), data = `"9"` = `0x39`.
`tag_plus_data = 1 (tag) + 1 (varint) + 1 (data) = 3`, so `backlen = [0x03]`.
Bytes: `FF 01 39 03` (4 bytes).

**Entry 3 -- field `Str(b"z")`:** same shape as entry 2: tag `FF`, varint
length `01`, data `"z"` = `0x7A`. `tag_plus_data = 3`, `backlen = [0x03]`.
Bytes: `FF 01 7A 03` (4 bytes).

**Entry 4 -- value `Str(b"ab")`:** tag = `FF`, varint length = 1 byte
(`0x02`), data = `"ab"` = `0x61 0x62`.
`tag_plus_data = 1 + 1 + 2 = 4`, so `backlen = [0x04]`.
Bytes: `FF 02 61 62 04` (5 bytes).

**Header:** `type = Hash (1)`, `format = 0`, `flags = 0`, `nentry = 4` (two
pairs), `tail_off = 12 (header) + 2 (entry 1) + 4 (entry 2) + 4 (entry 3) =
22 = 0x16` -- the start offset of entry 4, the last pair's *value* entry
(not the pair's first entry; `tail_off` always points at the block's
literal last entry).

Putting it all together, the 27-byte used block:

```
offset  bytes                        meaning
0       01 00 00 00 04 00 00 00      type=Hash, format=0, flags=0, nentry=4
        16 00 00 00                  tail_off=0x16=22
12      05 01                        field Uint(5)
14      FF 01 39 03                  value Str("9")
18      FF 01 7A 03                  field Str("z")
22      FF 02 61 62 04               value Str("ab")
```

## Format Evolution

`format` is owned by `type` -- there is no crate-wide format registry, only
each type's own `format = 0x00 | 0x01 | ...` space. The rule that keeps this
safe to extend is: **every `(type, format)` pair the crate emits must be
totally specified**, and any byte pair a decoder doesn't recognize is a
decode error, never silently reinterpreted. Concretely:

- `BlockHeader::parse` rejects any `format != 0` today (the only value any
  type currently defines) with `DecodeError::UnknownFormat`, and rejects
  any `type` outside `0..=3` with `DecodeError::UnknownType`.
- A new format for an existing type (e.g. the fixed-stride `List/0x01`
  sketched for uniform-integer lists) only has to teach that type's decoder
  about the new `format` byte; every other type's decoder, and the shared
  header/entry codec, is untouched.
- The chain-root flag bit (header `flags` bit 0) is parsed and validated
  today (an unset reserved bit is required; a set chain-root bit is
  accepted) but has no body semantics yet in any `format = 0x00` layout --
  it's reserved for a future chained/directory body format, deliberately
  unplanned here.

## API Notes

- **`*Mut` types are init-only.** `HashMut::init`, `ListMut::init`,
  `SetMut::init`, `ZsetMut::init` (and `BlockMut::parse` underneath them)
  each write or validate a *fresh* empty block of their type; there is no
  `HashMut::parse`-style constructor that re-wraps an already-populated
  buffer as a specific typed mutator. `BlockMut` itself only exposes raw
  splice primitives on top of the generic header -- there is currently **no
  path** from a populated buffer to a typed `HashMut`/`ListMut`/`SetMut`/
  `ZsetMut` view; parsing as `Block`/`BlockMut` and dispatching on
  `header().type_` does not produce one. Typed wrap-existing-buffer
  constructors do not exist yet; they are **required** for the pelikan
  entrystore integration (Plan C), which needs to mutate blocks it did not
  just create. When they land, what to re-validate on re-entry (the type
  tag, even-`nentry` parity for hash/zset, and whether pairing/sortedness is
  trusted from the stored bytes or re-checked) is a Plan C design decision,
  not something this crate has settled yet.
- **Callback pops.** `ListMut::pop_front`/`pop_back` take `f: impl
  FnOnce(EntryVal) -> R` instead of returning `Option<EntryVal>` directly.
  An `EntryVal::Str` borrows the block's backing bytes, but removing the
  entry memmoves those same bytes -- so the borrow has to end *before* the
  removal runs, while the value still has to be read *before* removal
  (removal is what invalidates it). `forbid(unsafe_code)` and `no_std` (no
  allocation to copy into) rule out the usual escapes, so the popped value
  is instead handed to `f` while the borrow is live, and the actual
  `remove_at` only runs once `f` returns.
- **The used-length slice contract.** Every cursor/locate call
  (`Cursor::first`/`last`/`next`/`prev`, `locate`) must be given a buffer
  sliced to the block's *used* length -- exactly `Block::bytes()`/
  `BlockMut::bytes()`, never `BlockMut::bytes_full()`. None of those
  functions know the backing buffer's full capacity, so an over-long slice
  can't be distinguished from "ran off the real tail" -- it can instead walk
  into stale bytes left in a `BlockMut`'s spare capacity (e.g. after a
  shrinking splice) and return a bogus "next" entry instead of `None`. This
  is a correctness precondition, not a memory-safety one: every decode is
  `.get()`-based with checked arithmetic, so out-of-bounds reads are
  impossible either way. `bytes_full()` exists only to snapshot/inspect raw
  storage (e.g. asserting a failed op left it byte-for-byte unchanged in a
  test), never to feed a cursor.
- **`*Mut::bytes()`.** `HashMut`/`ListMut`/`SetMut`/`ZsetMut` each expose a
  `bytes()` method with the same used-length contract as
  `BlockMut::bytes()` (a thin delegation to the private `BlockMut` each
  wraps). It exists for external callers -- the `typed_ops` fuzz target,
  differential testing -- that need to independently re-validate the block
  via `Block::parse` without reaching into the crate's internals; ordinary
  callers driving a collection through its typed ops don't need it.
- **Write ops report exact capacity needs.** Every mutator that can grow
  the block (`insert_at`, `replace_at`, and everything built on them:
  `hset`, `sadd`, `push_front`/`push_back`, `zadd`, ...) returns
  `Result<Fit, NeedBytes(usize)>`: `Fit` means the op completed in place
  within the buffer's current length; `NeedBytes(n)` means it needs a
  buffer of at least `n` bytes total and -- critically -- leaves the buffer
  byte-for-byte unmodified, so a caller can re-encode into a larger buffer
  and retry without having to reconstruct any partial state.

## Verification

Stacked from proof to sampling, matching the design spec's verification
plan:

1. **Kani** (`src/kani.rs`, `#[cfg(kani)]`, run via `cargo kani -p
   ziplist`): entry `Uint` codec roundtrip for all `u64`; header parsing
   never panics on arbitrary 16-byte buffers; `decode_backlen` never reads
   outside `[0, end)` for arbitrary buffers up to 32 bytes; a single
   `insert_at` on a symbolic <=3-entry list block keeps `nentry`/`tail_off`
   consistent (buffer <=96 bytes, entries restricted to `Uint` -- see the
   harness's own doc comment for the full bound rationale).
2. **Model-based proptest** (`tests/model.rs`): random op sequences per
   type against a plain `std` reference (`VecDeque`/`BTreeMap`/
   `BTreeSet`/`HashMap` with the crate's own comparator), asserting full
   observable-state equality after every op, including the
   capacity-refusal-leaves-model-unchanged path.
3. **cargo-fuzz** (`fuzz/`, targets `decode`, `ops`, `typed_ops`): `decode`
   throws arbitrary bytes at `BlockHeader::parse`/`Block::parse`, asserting
   no panic and no OOB read (the latter caught by the sanitizer); `ops`
   drives a structured, `arbitrary`-derived stream of `insert_at`/
   `remove_at`/`replace_at` calls directly against `BlockMut` (independent
   of any type's pairing convention), asserting the block re-parses
   cleanly via `Block::parse` after every single op; `typed_ops` drives the
   same kind of structured, coverage-guided op stream through the typed
   `HashMut`/`ListMut`/`SetMut`/`ZsetMut` wrappers instead -- `hset`/
   `hget`/`hdel`/`hincrby`, `sadd`/`srem`/`sismember`, `zadd`/`zrem`/
   `zscore`/`zincrby`/`zcount`/`zrange_by_rank`/`zrange_by_score`,
   `push_front`/`push_back`/`pop_front`/`pop_back`/`index`/`trim`/`range`,
   with a fuzzer-controlled buffer size (64..=4096 bytes, so `NeedBytes`
   refusals fire routinely) -- reaching the pairing/sort/delta-arithmetic
   layer (`pair_seek`, the zset's linear `find_member` scan, `hincrby`/
   `zincrby`'s over/underflow handling) that `ops` deliberately skips and
   that the model-based proptests above exercise only with a narrow, fixed
   byte-string strategy. Every op asserts the block re-parses via
   `Block::parse` on the type's own `bytes()`, and for the two pair types
   (hash, zset) that `nentry` stayed even.
4. **Golden bytes** (`tests/golden.rs`): one fixed, fully-worked input per
   type, asserting the exact encoded bytes -- freezing `(type, 0x00)`; any
   future diff there is a format break. The hash case is reproduced byte by
   byte above.
5. **Benchmarks** (`benches/codec.rs`, criterion): raw `encode_into`/
   `decode` throughput across entry shapes, plus per-op cost sweeps for
   `hget`/`hset`/`push_back`/`zadd` at `nentry` in `{8, 64, 512, 4096}` on a
   64KB buffer -- the codec-side half of the design spec's write-
   amplification seam sweep (the in-place-update-vs-rewrite half of that
   ablation needs `update`, which lands with the engine layer, not here).

### Running the fuzz targets and Kani locally

```bash
# fuzz (needs a nightly toolchain + cargo-fuzz: `cargo install cargo-fuzz`)
cd crates/data-structure/ziplist/fuzz
cargo +nightly fuzz run decode -- -runs=100000
cargo +nightly fuzz run ops -- -runs=100000
cargo +nightly fuzz run typed_ops -- -runs=100000

# kani (needs the kani-verifier tool: `cargo install --locked kani-verifier`
# followed by a one-time `cargo kani setup`)
cargo kani -p ziplist
```

CI runs both as a smoke check on every push/PR (short `-runs`/default
bounds, not the full local run above) -- see `.github/workflows/fuzz.yml`
and `.github/workflows/kani.yml`.
