# keyvalue: Shared Item Types

## Purpose

keyvalue provides the common key-value item representation shared across the workspace. It contains the packed item header, raw byte-level item access, and value types. Segcache uses these types for items packed within segments; any future cache engine in the workspace can reuse the same item format without duplication.

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
