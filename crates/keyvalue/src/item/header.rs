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
//! FLAGS: [is_numeric:1][is_deleted:1][optional_len:6]
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
pub const BASIC_HDR_SIZE: usize = std::mem::size_of::<BasicHeader>();

/// Size of the integrity fields (magic + CRC32) when the feature is enabled.
#[cfg(feature = "integrity")]
pub const BASIC_INTEGRITY_SIZE: usize = 2 + 4; // magic(2) + crc32(4)

#[cfg(not(feature = "integrity"))]
#[allow(dead_code)]
pub const BASIC_INTEGRITY_SIZE: usize = 0;

// Flag masks within the `flags` byte.
const NUMERIC_MASK: u8 = 0b1000_0000;
const DELETE_MASK: u8 = 0b0100_0000;
const OPT_MASK: u8 = 0b0011_1111;

/// Packed item header stored at the start of each item in segment memory.
///
/// Base layout: `[klen:1][flags:1][vlen:4]` = 6 bytes.
/// With `integrity`: `[magic:2][klen:1][flags:1][vlen:4][crc32:4]` = 12 bytes.
#[repr(C, packed)]
pub struct BasicHeader {
    #[cfg(feature = "integrity")]
    magic: [u8; 2],
    klen: u8,
    flags: u8,
    vlen: u32,
    #[cfg(feature = "integrity")]
    crc32: u32,
}

#[cfg(not(feature = "integrity"))]
const _: () = assert!(std::mem::size_of::<BasicHeader>() == 6);
#[cfg(feature = "integrity")]
const _: () = assert!(std::mem::size_of::<BasicHeader>() == 12);

impl BasicHeader {
    pub const MAGIC0: u8 = 0xCA;
    pub const MAGIC1: u8 = 0xFE;

    pub fn init(&mut self) {
        self.klen = 0;
        self.flags = 0;
        self.vlen = 0;
        #[cfg(feature = "integrity")]
        {
            self.magic = [Self::MAGIC0, Self::MAGIC1];
            self.crc32 = 0;
        }
    }

    /// # Panics
    /// Panics if the magic bytes are incorrect, indicating data corruption.
    pub fn check_magic(&self) {
        #[cfg(feature = "integrity")]
        {
            let magic = self.magic;
            assert_eq!(
                magic,
                [Self::MAGIC0, Self::MAGIC1],
                "item magic mismatch: expected {:02X?}, got {:02X?}",
                [Self::MAGIC0, Self::MAGIC1],
                magic,
            );
        }
    }

    #[cfg(feature = "integrity")]
    pub fn set_crc32(&mut self, crc: u32) {
        self.crc32 = crc;
    }

    #[cfg(feature = "integrity")]
    pub fn crc32(&self) -> u32 {
        self.crc32
    }

    // -- Key length --

    #[inline]
    pub fn key_len(&self) -> u8 {
        self.klen
    }

    #[inline]
    pub fn set_key_len(&mut self, klen: u8) {
        self.klen = klen;
    }

    // -- Value length --

    #[inline]
    pub fn value_len(&self) -> u32 {
        self.vlen
    }

    #[inline]
    pub fn set_value_len(&mut self, vlen: u32) {
        self.vlen = vlen;
    }

    // -- Optional data length (6 bits, max 63) --

    #[inline]
    pub fn optional_len(&self) -> u8 {
        self.flags & OPT_MASK
    }

    #[inline]
    pub fn set_optional_len(&mut self, olen: u8) {
        debug_assert!(olen <= OPT_MASK, "optional_len exceeds 6-bit max (63)");
        self.flags = (self.flags & !OPT_MASK) | (olen & OPT_MASK);
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

    // -- Deleted flag --

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & DELETE_MASK != 0
    }

    #[inline]
    pub fn set_deleted(&mut self, deleted: bool) {
        if deleted {
            self.flags |= DELETE_MASK;
        } else {
            self.flags &= !DELETE_MASK;
        }
    }
}

impl std::fmt::Debug for BasicHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicHeader")
            .field("key_len", &self.key_len())
            .field("value_len", &self.value_len())
            .field("optional_len", &self.optional_len())
            .field("is_numeric", &self.is_numeric())
            .field("is_deleted", &self.is_deleted())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed() -> BasicHeader {
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn is_deleted_roundtrip() {
        let mut h = zeroed();
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
        h.set_optional_len(5);
        assert!(h.is_deleted(), "is_deleted should survive set_numeric and set_optional_len");
        assert!(h.is_numeric());
        assert_eq!(h.optional_len(), 5);
    }

    #[test]
    fn set_numeric_does_not_clear_deleted() {
        let mut h = zeroed();
        h.set_deleted(true);
        h.set_numeric(false);
        assert!(h.is_deleted());
    }
}
