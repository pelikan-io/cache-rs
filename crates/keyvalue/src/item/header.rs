//! Item header with byte-aligned field layout.
//!
//! Each item in a segment begins with this header, followed by optional data,
//! key bytes, and value bytes.
//!
//! ```text
//! ┌──────┬───────┬──────────────────────────────┐
//! │ KLEN │ FLAGS │             VLEN             │
//! │  u8  │  u8   │             u32              │
//! │ 8bit │ 8bit  │            32 bit            │
//! └──────┴───────┴──────────────────────────────┘
//!
//! FLAGS: [is_numeric:1][is_deleted:1][olen:6]
//!
//! With `integrity` feature, magic and CRC32 fields are added:
//!
//! ┌───────┬───────┬──────┬───────┬──────────────┬──────────────┐
//! │MAGIC_0│MAGIC_1│ KLEN │ FLAGS │     VLEN     │    CRC32     │
//! │  u8   │  u8   │  u8  │  u8   │     u32      │     u32      │
//! │ 0xCA  │ 0xFE  │ 8bit │ 8bit  │    32 bit    │    32 bit    │
//! └───────┴───────┴──────┴───────┴──────────────┴──────────────┘
//!
//! The CRC32 covers the full item: magic + klen + flags + vlen + optional
//! + key + value. Computed with the CRC32 field zeroed during calculation.
//! ```

use core::sync::atomic::{AtomicU8, Ordering};

/// The size of the item header in bytes.
pub const ITEM_HDR_SIZE: usize = std::mem::size_of::<ItemHeader>();

/// Magic sentinel bytes for integrity checking.
#[cfg(feature = "integrity")]
pub const ITEM_MAGIC: [u8; 2] = [0xCA, 0xFE];

/// Size of the integrity fields (magic + CRC32) when the feature is enabled.
#[cfg(feature = "integrity")]
pub const ITEM_INTEGRITY_SIZE: usize = 2 + 4; // magic(2) + crc32(4)

#[cfg(not(feature = "integrity"))]
#[allow(dead_code)]
pub const ITEM_INTEGRITY_SIZE: usize = 0;

// Flag masks within the `flags` byte.
const NUMERIC_MASK: u8 = 0b1000_0000;
const DELETE_MASK: u8 = 0b0100_0000;
const OLEN_MASK: u8 = 0b0011_1111;

/// Packed item header stored at the start of each item in segment memory.
///
/// Base layout: `[klen:1][flags:1][vlen:4]` = 6 bytes.
/// With `integrity`: `[magic:2][klen:1][flags:1][vlen:4][crc32:4]` = 12 bytes.
///
/// All fields are directly byte-addressable — no cross-word bit manipulation.
///
/// # Concurrency
///
/// `flags` is the ONE header byte that is written after an item is
/// published: `set_deleted` marks a live, reader-visible item. Every other
/// field is written only during `define` on unpublished memory. The flags
/// byte is therefore an `AtomicU8` (align 1, so `packed` layout and size
/// are unchanged): `set_deleted` is an atomic RMW taking `&self`, and the
/// flag readers (`olen`, `is_numeric`, `is_deleted`) are atomic loads —
/// without this, a reader decoding `olen` out of the flags byte races the
/// delete's write on the same byte (a TSan-visible data race). `Relaxed`
/// suffices throughout: the flag carries no payload of its own — the
/// surrounding publish/pin protocol (Release slot CAS / SeqCst pins)
/// provides all cross-field ordering.
///
/// Not `packed`: `AtomicU8` carries a `repr(align)` marker that packed
/// structs reject, so instead every field is naturally align-1 (`vlen` and
/// `crc32` as native-endian byte arrays behind accessors) — same bytes,
/// same 6/12-byte layout, checked by the size asserts below.
#[repr(C)]
pub struct ItemHeader {
    #[cfg(feature = "integrity")]
    magic: [u8; 2],
    klen: u8,
    flags: AtomicU8,
    vlen: [u8; 4],
    #[cfg(feature = "integrity")]
    crc32: [u8; 4],
}

// Verify expected sizes at compile time.
#[cfg(not(feature = "integrity"))]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 6);
#[cfg(feature = "integrity")]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 12);

impl ItemHeader {
    /// Initialize header fields to zero (and set magic if enabled).
    pub fn init(&mut self) {
        self.klen = 0;
        *self.flags.get_mut() = 0;
        self.vlen = [0; 4];
        #[cfg(feature = "integrity")]
        {
            self.magic = ITEM_MAGIC;
            self.crc32 = [0; 4];
        }
    }

    /// Check that the magic bytes match the expected value.
    ///
    /// # Panics
    /// Panics if the magic bytes are incorrect, indicating data corruption.
    pub fn check_magic(&self) {
        #[cfg(feature = "integrity")]
        {
            let magic = self.magic;
            assert_eq!(
                magic, ITEM_MAGIC,
                "item magic mismatch: expected {:02X?}, got {:02X?}",
                ITEM_MAGIC, magic,
            );
        }
    }

    /// Store the CRC32 value in the header (native-endian bytes).
    #[cfg(feature = "integrity")]
    pub fn set_crc32(&mut self, crc: u32) {
        self.crc32 = crc.to_ne_bytes();
    }

    /// Get the stored CRC32 value.
    #[cfg(feature = "integrity")]
    pub fn crc32(&self) -> u32 {
        u32::from_ne_bytes(self.crc32)
    }

    // -- Key length --

    #[inline]
    pub fn klen(&self) -> u8 {
        self.klen
    }

    #[inline]
    pub fn set_klen(&mut self, klen: u8) {
        self.klen = klen;
    }

    // -- Value length --

    #[inline]
    pub fn vlen(&self) -> u32 {
        u32::from_ne_bytes(self.vlen)
    }

    #[inline]
    pub fn set_vlen(&mut self, vlen: u32) {
        self.vlen = vlen.to_ne_bytes();
    }

    // -- Optional data length (6 bits, max 63) --

    #[inline]
    pub fn olen(&self) -> u8 {
        self.flags.load(Ordering::Relaxed) & OLEN_MASK
    }

    #[inline]
    pub fn set_olen(&mut self, olen: u8) {
        debug_assert!(olen <= OLEN_MASK, "olen exceeds 6-bit max (63)");
        let flags = self.flags.get_mut();
        *flags = (*flags & !OLEN_MASK) | (olen & OLEN_MASK);
    }

    // -- Numeric flag --

    #[inline]
    pub fn is_numeric(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & NUMERIC_MASK != 0
    }

    #[inline]
    pub fn set_numeric(&mut self, numeric: bool) {
        // define-time only (unpublished memory, exclusive) — plain write.
        if numeric {
            *self.flags.get_mut() |= NUMERIC_MASK;
        } else {
            *self.flags.get_mut() &= !NUMERIC_MASK;
        }
    }

    // -- Deleted flag --

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & DELETE_MASK != 0
    }

    /// The raw flags byte, read atomically — for code that must hash or
    /// copy header bytes while a concurrent `set_deleted` may be flipping
    /// the delete bit (the CRC computations). A plain byte read of this
    /// field would be the same data race the atomic accessors exist to
    /// avoid.
    #[cfg(feature = "integrity")]
    #[inline]
    pub(crate) fn flags_byte(&self) -> u8 {
        self.flags.load(Ordering::Relaxed)
    }

    /// Byte offset of the flags byte within the header, for the CRC
    /// hashers that must splice an atomically-loaded flags byte into the
    /// plain header prefix. Pinned by `field_offsets_are_the_packed_layout`.
    #[cfg(feature = "integrity")]
    pub(crate) const FLAGS_OFFSET: usize = 3;

    /// Mark or unmark the item deleted. `&self` and atomic by design:
    /// this is the one header mutation performed on a PUBLISHED item, so
    /// it must neither race flag readers non-atomically nor manufacture
    /// an aliasing `&mut` over reader-shared memory. The RMW touches only
    /// the delete bit — a concurrent reader's `olen`/`is_numeric` decode
    /// of the same byte is unaffected.
    #[inline]
    pub fn set_deleted(&self, deleted: bool) {
        if deleted {
            self.flags.fetch_or(DELETE_MASK, Ordering::Relaxed);
        } else {
            self.flags.fetch_and(!DELETE_MASK, Ordering::Relaxed);
        }
    }
}

impl std::fmt::Debug for ItemHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemHeader")
            .field("klen", &self.klen())
            .field("vlen", &self.vlen())
            .field("olen", &self.olen())
            .field("is_numeric", &self.is_numeric())
            .field("is_deleted", &self.is_deleted())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed() -> ItemHeader {
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn is_deleted_roundtrip() {
        let h = zeroed();
        assert!(!h.is_deleted());
        h.set_deleted(true);
        assert!(h.is_deleted());
        h.set_deleted(false);
        assert!(!h.is_deleted());
    }

    #[test]
    fn is_deleted_independent_of_other_flags() {
        let mut h = zeroed();
        h.set_deleted(true);
        h.set_numeric(true);
        h.set_olen(5);
        assert!(
            h.is_deleted(),
            "is_deleted should survive set_numeric and set_olen"
        );
        assert!(h.is_numeric());
        assert_eq!(h.olen(), 5);
    }

    #[test]
    fn set_numeric_does_not_clear_deleted() {
        let mut h = zeroed();
        h.set_deleted(true);
        h.set_numeric(false);
        assert!(h.is_deleted());
    }

    /// Byte-level layout pin. The struct stopped being `packed` when the
    /// flags byte became atomic; `repr(C)` with all-align-1 fields must
    /// keep the exact `[magic?][klen][flags][vlen][crc32?]` byte positions
    /// (segment memory is parsed at these offsets).
    #[test]
    fn field_offsets_are_the_packed_layout() {
        let mut h = zeroed();
        h.set_klen(0xAB);
        h.set_olen(0x15); // 0b01_0101 in the flags byte's low six bits
        h.set_numeric(true);
        h.set_vlen(0x1234_5678);

        let bytes = unsafe {
            std::slice::from_raw_parts(&h as *const ItemHeader as *const u8, ITEM_HDR_SIZE)
        };
        let base = if cfg!(feature = "integrity") { 2 } else { 0 };
        #[cfg(feature = "integrity")]
        assert_eq!(
            base + 1,
            ItemHeader::FLAGS_OFFSET,
            "FLAGS_OFFSET must track the flags byte's position"
        );
        assert_eq!(bytes[base], 0xAB, "klen byte");
        assert_eq!(bytes[base + 1], NUMERIC_MASK | 0x15, "flags byte");
        assert_eq!(
            &bytes[base + 2..base + 6],
            &0x1234_5678u32.to_ne_bytes(),
            "vlen bytes"
        );
    }

    /// The delete tombstone is written on a PUBLISHED item, so it must be
    /// callable through `&self` while other threads decode the same flags
    /// byte. The assertions are the invariants; the real referee is TSan,
    /// which flagged the pre-atomic version of exactly this pattern.
    #[test]
    fn set_deleted_races_flag_readers_safely() {
        let h = zeroed();
        let h = &h;
        std::thread::scope(|s| {
            let writer = s.spawn(move || {
                for _ in 0..10_000 {
                    h.set_deleted(true);
                    h.set_deleted(false);
                }
                h.set_deleted(true);
            });
            for _ in 0..3 {
                s.spawn(move || {
                    for _ in 0..10_000 {
                        // olen and is_numeric decode the byte the writer
                        // is flipping; the delete bit must never bleed.
                        assert_eq!(h.olen(), 0);
                        assert!(!h.is_numeric());
                        let _ = h.is_deleted();
                    }
                });
            }
            writer.join().unwrap();
        });
        assert!(h.is_deleted());
    }
}
