//! Item header with byte-aligned field layout.
//!
//! Each item in a segment begins with this header, followed by optional data,
//! key bytes, and value bytes.
//!
//! ```text
//! ┌──────┬───────┬──────────────────────────────┐
//! │ KLEN │ FLAGS │             VLEN             │
//! │ u8   │ u8    │             u32              │
//! │ 8bit │ 8bit  │            32 bit            │
//! └──────┴───────┴──────────────────────────────┘
//!
//! FLAGS: [is_numeric:1][reserved:1][olen:6]
//!
//! With `magic` feature, a u32 magic field (0xDECAFBAD) is appended:
//!
//! ┌──────┬───────┬──────────────────────────────┬──────────────┐
//! │ KLEN │ FLAGS │             VLEN             │    MAGIC     │
//! │ u8   │ u8    │             u32              │     u32      │
//! │ 8bit │ 8bit  │            32 bit            │    32 bit    │
//! └──────┴───────┴──────────────────────────────┴──────────────┘
//! ```

/// The size of the item header in bytes.
pub const ITEM_HDR_SIZE: usize = std::mem::size_of::<ItemHeader>();

/// Magic value written into each item header for corruption detection.
#[cfg(feature = "magic")]
pub const ITEM_MAGIC: u32 = 0xDECAFBAD;

/// The size of the magic field in bytes (4 when enabled, 0 when not).
#[cfg(feature = "magic")]
pub const ITEM_MAGIC_SIZE: usize = std::mem::size_of::<u32>();

#[cfg(not(feature = "magic"))]
#[allow(dead_code)]
pub const ITEM_MAGIC_SIZE: usize = 0;

// Flag masks within the `flags` byte.
const NUMERIC_MASK: u8 = 0b1000_0000;
const OLEN_MASK: u8 = 0b0011_1111;

/// Packed item header stored at the start of each item in segment memory.
///
/// Layout: `[klen:1][flags:1][vlen:4]` = 6 bytes.
/// With `magic` feature: `[klen:1][flags:1][vlen:4][magic:4]` = 10 bytes.
///
/// All fields are directly byte-addressable — no cross-word bit manipulation.
#[repr(C, packed)]
pub struct ItemHeader {
    klen: u8,
    flags: u8,
    vlen: u32,
    #[cfg(feature = "magic")]
    magic: u32,
}

// Verify expected sizes at compile time.
#[cfg(not(feature = "magic"))]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 6);
#[cfg(feature = "magic")]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 10);

impl ItemHeader {
    /// Initialize header fields to zero (and set magic if enabled).
    pub fn init(&mut self) {
        self.klen = 0;
        self.flags = 0;
        self.vlen = 0;
        #[cfg(feature = "magic")]
        {
            self.magic = ITEM_MAGIC;
        }
    }

    /// Check that the magic bytes match the expected value.
    ///
    /// # Panics
    /// Panics if the magic bytes are incorrect, indicating data corruption.
    pub fn check_magic(&self) {
        #[cfg(feature = "magic")]
        {
            // Copy out of packed struct to avoid unaligned reference.
            let magic = self.magic;
            assert_eq!(
                magic, ITEM_MAGIC,
                "item magic mismatch: expected 0x{ITEM_MAGIC:08X}, got 0x{magic:08X}",
            );
        }
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
