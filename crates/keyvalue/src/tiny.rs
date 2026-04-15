//! A compact item representation for fixed-size slot caches.
//!
//! [`TinyItem`] matches the item layout used by Pelikan's cuckoo storage
//! engine. The header is only 6 bytes — an expiration timestamp, a key
//! length, and a value length — followed directly by key and value bytes.
//!
//! ```text
//! ┌──────────┬──────┬──────┬──────────┬──────────┐
//! │  EXPIRE  │ KLEN │ VLEN │   KEY    │  VALUE   │
//! │  (u32)   │ (u8) │ (u8) │          │          │
//! │ 4 bytes  │ 1 b  │ 1 b  │          │          │
//! └──────────┴──────┴──────┴──────────┴──────────┘
//! ```
//!
//! Integer values are signalled by `vlen == 0` (the actual storage is always
//! 8 bytes for a big-endian `u64`). An empty/unused slot is indicated by
//! `expire == 0`.

use crate::{NotNumericError, Value};

/// Size of the [`TinyItemHeader`] in bytes.
pub const TINY_ITEM_HDR_SIZE: usize = std::mem::size_of::<TinyItemHeader>();

/// Packed per-item header stored at the start of every slot.
#[repr(C, packed)]
pub struct TinyItemHeader {
    expire: u32,
    klen: u8,
    vlen: u8,
}

impl TinyItemHeader {
    /// Expiration timestamp (0 = empty slot).
    #[inline]
    pub fn expire(&self) -> u32 {
        self.expire
    }

    /// Set the expiration timestamp.
    #[inline]
    pub fn set_expire(&mut self, expire: u32) {
        self.expire = expire;
    }

    /// Key length in bytes.
    #[inline]
    pub fn klen(&self) -> u8 {
        self.klen
    }

    /// Raw value-length field. Returns 0 for integer values.
    #[inline]
    pub fn raw_vlen(&self) -> u8 {
        self.vlen
    }

    /// Actual number of value bytes stored.
    #[inline]
    pub fn value_len(&self) -> usize {
        if self.vlen == 0 {
            std::mem::size_of::<u64>()
        } else {
            self.vlen as usize
        }
    }
}

impl std::fmt::Debug for TinyItemHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyItemHeader")
            .field("expire", &self.expire())
            .field("klen", &self.klen())
            .field("vlen", &self.raw_vlen())
            .finish()
    }
}

/// A compact item stored as `[TinyItemHeader][key][value]` in a fixed-size
/// slot. Wraps a raw pointer into the backing buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TinyItem {
    data: *mut u8,
}

impl TinyItem {
    /// Create a `TinyItem` from a pointer to the start of a slot.
    ///
    /// # Safety
    ///
    /// The pointer must be within a valid allocation with at least
    /// `TINY_ITEM_HDR_SIZE` bytes available.
    pub fn from_ptr(ptr: *mut u8) -> Self {
        Self { data: ptr }
    }

    /// Immutable reference to the header.
    #[inline]
    pub fn header(&self) -> &TinyItemHeader {
        unsafe { &*(self.data as *const TinyItemHeader) }
    }

    /// Mutable pointer to the header.
    #[inline]
    fn header_mut(&self) -> *mut TinyItemHeader {
        self.data as *mut TinyItemHeader
    }

    /// Expiration timestamp (0 = empty slot).
    #[inline]
    pub fn expire(&self) -> u32 {
        self.header().expire()
    }

    /// Key length.
    #[inline]
    pub fn klen(&self) -> u8 {
        self.header().klen()
    }

    /// Borrow the key bytes.
    pub fn key(&self) -> &[u8] {
        unsafe {
            let ptr = self.data.add(TINY_ITEM_HDR_SIZE);
            std::slice::from_raw_parts(ptr, self.klen() as usize)
        }
    }

    /// Borrow the value.
    pub fn value(&self) -> Value<'_> {
        let vlen = self.header().raw_vlen();
        let off = TINY_ITEM_HDR_SIZE + self.klen() as usize;

        if vlen == 0 {
            let bytes = unsafe { std::slice::from_raw_parts(self.data.add(off), 8) };
            Value::U64(u64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(self.data.add(off), vlen as usize) };
            Value::Bytes(bytes)
        }
    }

    /// Write key, value, and expiration into the slot.
    pub fn define(&mut self, key: &[u8], value: Value, expire: u32) {
        unsafe {
            let hdr = &mut *self.header_mut();
            hdr.expire = expire;
            hdr.klen = key.len() as u8;

            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.data.add(TINY_ITEM_HDR_SIZE),
                key.len(),
            );

            let val_off = TINY_ITEM_HDR_SIZE + key.len();
            match value {
                Value::Bytes(v) => {
                    hdr.vlen = v.len() as u8;
                    std::ptr::copy_nonoverlapping(v.as_ptr(), self.data.add(val_off), v.len());
                }
                Value::U64(v) => {
                    hdr.vlen = 0;
                    let bytes = v.to_be_bytes();
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        self.data.add(val_off),
                        bytes.len(),
                    );
                }
            }
        }
    }

    /// Perform a wrapping addition on a u64 value.
    pub fn wrapping_add(&mut self, rhs: u64) -> Result<(), NotNumericError> {
        match self.value() {
            Value::U64(v) => unsafe {
                let off = TINY_ITEM_HDR_SIZE + self.klen() as usize;
                let new = v.wrapping_add(rhs);
                std::ptr::copy_nonoverlapping(
                    new.to_be_bytes().as_ptr(),
                    self.data.add(off),
                    std::mem::size_of::<u64>(),
                );
                Ok(())
            },
            _ => Err(NotNumericError),
        }
    }

    /// Perform a saturating subtraction on a u64 value.
    pub fn saturating_sub(&mut self, rhs: u64) -> Result<(), NotNumericError> {
        match self.value() {
            Value::U64(v) => unsafe {
                let off = TINY_ITEM_HDR_SIZE + self.klen() as usize;
                let new = v.saturating_sub(rhs);
                std::ptr::copy_nonoverlapping(
                    new.to_be_bytes().as_ptr(),
                    self.data.add(off),
                    std::mem::size_of::<u64>(),
                );
                Ok(())
            },
            _ => Err(NotNumericError),
        }
    }
}

impl std::fmt::Debug for TinyItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyItem")
            .field("header", self.header())
            .finish()
    }
}
