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
//! FLAGS: [is_numeric:1][reserved:1][olen:6]
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
const OLEN_MASK: u8 = 0b0011_1111;

/// Packed item header stored at the start of each item in segment memory.
///
/// Base layout: `[klen:1][flags:1][vlen:4]` = 6 bytes.
/// With `integrity`: `[magic:2][klen:1][flags:1][vlen:4][crc32:4]` = 12 bytes.
///
/// All fields are directly byte-addressable — no cross-word bit manipulation.
#[repr(C, packed)]
pub struct ItemHeader {
    #[cfg(feature = "integrity")]
    magic: [u8; 2],
    klen: u8,
    flags: u8,
    vlen: u32,
    #[cfg(feature = "integrity")]
    crc32: u32,
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
        self.flags = 0;
        self.vlen = 0;
        #[cfg(feature = "integrity")]
        {
            self.magic = ITEM_MAGIC;
            self.crc32 = 0;
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

    /// Store the CRC32 value in the header.
    #[cfg(feature = "integrity")]
    pub fn set_crc32(&mut self, crc: u32) {
        self.crc32 = crc;
    }

    /// Get the stored CRC32 value.
    #[cfg(feature = "integrity")]
    pub fn crc32(&self) -> u32 {
        self.crc32
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
        self.vlen
    }

    #[inline]
    pub fn set_vlen(&mut self, vlen: u32) {
        self.vlen = vlen;
    }

    // -- Optional data length (6 bits, max 63) --

    #[inline]
    pub fn olen(&self) -> u8 {
        self.flags & OLEN_MASK
    }

    #[inline]
    pub fn set_olen(&mut self, olen: u8) {
        debug_assert!(olen <= OLEN_MASK, "olen exceeds 6-bit max (63)");
        self.flags = (self.flags & !OLEN_MASK) | (olen & OLEN_MASK);
    }

    // -- Numeric flag --

    #[inline]
    pub fn is_numeric(&self) -> bool {
        self.flags & NUMERIC_MASK != 0
    }

    #[inline]
    pub fn set_numeric(&mut self, numeric: bool) {
        if numeric {
            self.flags |= NUMERIC_MASK;
        } else {
            self.flags &= !NUMERIC_MASK;
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
            .finish()
    }
}
