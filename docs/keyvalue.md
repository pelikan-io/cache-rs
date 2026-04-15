# keyvalue: Shared Item Types

## Purpose

keyvalue provides the common key-value item representations shared across the workspace. It contains packed item headers, raw byte-level item access, and value types. Segcache uses `RawItem` for items packed within segments; cuckoo-cache uses `TinyItem` for fixed-size slot storage.

## What's Inside

### Value / OwnedValue

The value enum that cache entries hold:

```rust
pub enum Value<'a> {
    Bytes(&'a [u8]),
    U64(u64),
}
```

`Value` borrows data; `OwnedValue` owns it. Conversions exist for `&[u8]`, `&str`, `u64`, `&Vec<u8>`, and fixed-size byte arrays. Comparison operators work across representations (`Value == &[u8]`, `Value == u64`).

### ItemHeader

A `#[repr(C, packed)]` struct encoding key length, value length, optional data length, and type flags into 5 bytes (9 with the `magic` feature):

```
┌──────────────────┬──────────┬──────┬───────┐
│  Magic? (4B)     │ VLen:24  │KLen:8│Flags:8│
└──────────────────┴──────────┴──────┴───────┘
```

The packed representation means `ItemHeader` is always read and written through raw pointer casts. Field access methods handle the bit manipulation.

### RawItem

A thin wrapper around a `*mut u8` pointing to a buffer laid out as `[ItemHeader][optional][key][value]`. Provides:

- `key() -> &[u8]`, `value() -> Value<'_>`, `optional() -> Option<&[u8]>`
- `define(key, value, optional)` — writes data into the buffer
- `wrapping_add(rhs)` / `saturating_sub(rhs)` — in-place arithmetic on numeric values
- `size() -> usize` — aligned item size
- `check_magic()` — validates corruption-detection magic bytes

Segcache wraps `RawItem` in its own `Item` struct that adds CAS values and maps `NotNumericError` to `SegcacheError`.

### TinyItem

A compact 6-byte header item matching the [pelikan-c cuckoo item layout](https://github.com/pelikan-io/pelikan-c/blob/main/src/storage/cuckoo/item.h). Wraps a `*mut u8` pointing to a buffer laid out as `[TinyItemHeader][key][value]`:

```
┌──────────┬──────┬──────┬──────────┬──────────┐
│  EXPIRE  │ KLEN │ VLEN │   KEY    │  VALUE   │
│  (u32)   │ (u8) │ (u8) │          │          │
│ 4 bytes  │ 1 b  │ 1 b  │          │          │
└──────────┴──────┴──────┴──────────┴──────────┘
```

Key differences from `RawItem`:

| | RawItem | TinyItem |
|--|---------|----------|
| Header size | 5 bytes (9 with magic) | 6 bytes |
| Expiration | Not stored (managed externally) | Built into header (`expire` field) |
| Value length | 24-bit (up to 16 MB) | 8-bit (up to 255 bytes) |
| Optional data | Supported (6-bit length) | Not supported |
| Integer values | Typed via flags bit | Signalled by `vlen == 0` |
| Empty detection | `klen == 0` | `expire == 0` |
| Magic bytes | Supported (`magic` feature) | Not supported |

TinyItem is designed for fixed-size slot caches where items are small and per-item overhead must be minimal. The 6-byte header packs expiration, key length, and value length with no wasted bits.

Provides:

- `expire() -> u32`, `klen() -> u8`, `key() -> &[u8]`, `value() -> Value<'_>`
- `define(key, value, expire)` — writes data into the buffer
- `wrapping_add(rhs)` / `saturating_sub(rhs)` — in-place arithmetic on numeric values

Cuckoo-cache wraps `TinyItem` in its own `Item` struct that maps `NotNumericError` to `CuckooCacheError`.

### NotNumericError

A simple unit error returned by `wrapping_add` / `saturating_sub` when the value isn't `U64`. The cache crate converts this to its own error variant via `map_err`.

## Feature Flags

| Feature | Effect |
|---------|--------|
| `magic` | Adds a 4-byte magic field (0xDECAFBAD) to `ItemHeader` |
| `debug` | Enables `magic` |

Segcache forwards its `magic` feature to keyvalue (`magic = ["keyvalue/magic"]`).

## Zero-Cost Sharing

All types are concrete structs with `#[inline]` accessors. No traits, no generics, no dynamic dispatch. The compiler monomorphizes everything identically to having the code inlined in the consumer crate.
