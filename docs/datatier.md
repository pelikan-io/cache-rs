# datatier: Byte Storage Pool Abstraction

## Purpose

datatier provides a `Datapool` trait that abstracts over different byte storage backends. Cache engines like segcache allocate their segment storage from a datapool, which decouples the caching logic from the storage medium.

## The Datapool Trait

```rust
pub trait Datapool: Send {
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
    fn flush(&mut self) -> Result<(), std::io::Error>;
    fn len(&self) -> usize;
}
```

A datapool is fundamentally a contiguous byte region that can be read, written, and flushed. The `Send` bound allows ownership transfer across threads, though the mutable borrow API means only one thread can access the data at a time.

## Implementations

### Memory

Anonymous mmap. All data is volatile -- lost on process exit. Pages are prefaulted on creation by writing a zero at the start of each page.

**Use case**: Standard volatile caching (the common case).

### MmapFile

File-backed mmap. The file contains a one-page header followed by the data region. On `flush()`, a blake3 checksum is computed over the header and data, then written into the header. On `open()`, the checksum is verified to detect corruption.

**Use case**: DAX-aware filesystems on persistent memory (e.g., Intel Optane). Can also be used for crash recovery on regular filesystems, though page cache interference makes this less efficient.

### FileBackedMemory

Hybrid: data lives in an anonymous mmap (like `Memory`) but is backed by a file for durability. On `flush()`, data is written page-by-page from memory to the file, checksummed, and synced. On `open()`, data is read back from the file into a fresh anonymous mmap.

**Use case**: Fast DRAM access with periodic persistence to local disk (e.g., NVMe). The code attempts `O_DIRECT` on Linux to avoid page cache pollution, though this is currently disabled pending fixes.

## Header Format

All file-backed implementations share a 4096-byte header (`repr(C, packed)`):

| Field | Size | Description |
|-------|------|-------------|
| checksum | 32 bytes | blake3 hash of header (with checksum zeroed) + data |
| magic | 8 bytes | `b"PELIKAN!"` -- identifies valid headers |
| version | 8 bytes | Format version (currently 0) |
| timestamps | 32 bytes | Monotonic and wall-clock times at creation (coarse + precise) |
| user_version | 8 bytes | Application-defined version for schema compatibility |
| options | 8 bytes | Reserved for future flags |
| padding | 4008 bytes | Fills to exactly one page |

The header occupies the first page of the file. Data starts at offset 4096. Total file size is always a whole number of pages.

## Page Alignment

All sizes are rounded up to page boundaries (4096 bytes). This is required for `O_DIRECT` compatibility and ensures efficient mmap behavior. The data region accessible through `as_slice()` may be slightly larger than requested due to this rounding.

## Checksum Verification

On `open()`, the stored checksum is compared against a freshly computed blake3 hash. The hash covers the header (with its checksum field zeroed) concatenated with the entire data region. Any bit flip in the header or data causes the open to fail with "checksum mismatch". This catches silent corruption, partial writes, and mismatched files.
