//! A raw byte-level representation of an item.
//!
//! The [`RawItem`] provides direct byte-level access to item data stored as
//! a packed buffer of `[ItemHeader][optional][key][value]`.

use crate::item::*;
use crate::NotNumericError;
use crate::Value;

/// The raw byte-level representation of an item.
///
/// This is a thin wrapper around a raw pointer to a packed item buffer.
/// The caller is responsible for ensuring the pointer is valid and properly
/// aligned.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawItem {
    data: *mut u8,
}

impl RawItem {
    /// Create a `RawItem` from a pointer.
    ///
    /// # Safety
    ///
    /// The pointer must point to a valid item buffer with a properly
    /// initialized [`ItemHeader`]. Undefined behavior results from
    /// passing an invalid or misaligned pointer.
    pub fn from_ptr(ptr: *mut u8) -> RawItem {
        Self { data: ptr }
    }

    /// Get an immutable reference to the item's header.
    pub fn header(&self) -> &ItemHeader {
        unsafe { &*(self.data as *const ItemHeader) }
    }

    /// Get a mutable pointer to the item's header.
    fn header_mut(&mut self) -> *mut ItemHeader {
        self.data as *mut ItemHeader
    }

    /// Returns the key length.
    #[inline]
    pub fn klen(&self) -> u8 {
        self.header().klen()
    }

    /// Borrow the key bytes.
    pub fn key(&self) -> &[u8] {
        unsafe {
            let ptr = self.data.add(self.key_offset());
            let len = self.klen() as usize;
            std::slice::from_raw_parts(ptr, len)
        }
    }

    /// Returns the value length as stored in the header.
    #[inline]
    fn vlen(&self) -> u32 {
        self.header().vlen()
    }

    /// Borrow the value, returning either bytes or a decoded u64.
    pub fn value(&self) -> Value<'_> {
        let bytes = unsafe {
            let ptr = self.data.add(self.value_offset());
            let len = self.vlen() as usize;
            std::slice::from_raw_parts(ptr, len)
        };

        if self.header().is_numeric() {
            Value::U64(u64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        } else {
            Value::Bytes(bytes)
        }
    }

    /// Returns the optional data length.
    #[inline]
    pub fn olen(&self) -> u8 {
        self.header().olen()
    }

    /// Borrow the optional data, if any.
    pub fn optional(&self) -> Option<&[u8]> {
        let olen = self.olen() as usize;
        if olen > 0 {
            unsafe {
                let ptr = self.data.add(self.optional_offset());
                Some(std::slice::from_raw_parts(ptr, olen))
            }
        } else {
            None
        }
    }

    /// Check the header magic bytes.
    #[inline]
    pub fn check_magic(&self) {
        self.header().check_magic()
    }

    /// Write key, value, and optional data into the item buffer.
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        unsafe {
            (*self.header_mut()).init();
            (*self.header_mut()).set_olen(optional.len() as u8);
            (*self.header_mut()).set_klen(key.len() as u8);

            // Copy optional data
            std::ptr::copy_nonoverlapping(
                optional.as_ptr(),
                self.data.add(self.optional_offset()),
                optional.len(),
            );

            // Copy key
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.data.add(self.key_offset()),
                key.len(),
            );

            // Copy value
            match value {
                Value::Bytes(v) => {
                    (*self.header_mut()).set_numeric(false);
                    (*self.header_mut()).set_vlen(v.len() as u32);
                    std::ptr::copy_nonoverlapping(
                        v.as_ptr(),
                        self.data.add(self.value_offset()),
                        v.len(),
                    );
                }
                Value::U64(v) => {
                    (*self.header_mut()).set_numeric(true);
                    let bytes = v.to_be_bytes();
                    (*self.header_mut()).set_vlen(bytes.len() as u32);
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        self.data.add(self.value_offset()),
                        bytes.len(),
                    );
                }
            }
        }
    }

    // -- Offset calculations --

    #[inline]
    fn optional_offset(&self) -> usize {
        ITEM_HDR_SIZE
    }

    #[inline]
    fn key_offset(&self) -> usize {
        self.optional_offset() + self.olen() as usize
    }

    #[inline]
    fn value_offset(&self) -> usize {
        self.key_offset() + self.klen() as usize
    }

    /// Returns item size, rounded up to 8-byte alignment.
    pub fn size(&self) -> usize {
        let raw =
            ITEM_HDR_SIZE + self.olen() as usize + self.klen() as usize + self.vlen() as usize;
        ((raw >> 3) + 1) << 3
    }

    /// Perform a wrapping addition on a numeric value.
    pub fn wrapping_add(&mut self, rhs: u64) -> Result<(), NotNumericError> {
        match self.value() {
            Value::U64(v) => unsafe {
                let new = v.wrapping_add(rhs);
                std::ptr::copy_nonoverlapping(
                    new.to_be_bytes().as_ptr(),
                    self.data.add(self.value_offset()),
                    core::mem::size_of::<u64>(),
                );
                Ok(())
            },
            _ => Err(NotNumericError),
        }
    }

    /// Perform a saturating subtraction on a numeric value.
    pub fn saturating_sub(&mut self, rhs: u64) -> Result<(), NotNumericError> {
        match self.value() {
            Value::U64(v) => unsafe {
                let new = v.saturating_sub(rhs);
                std::ptr::copy_nonoverlapping(
                    new.to_be_bytes().as_ptr(),
                    self.data.add(self.value_offset()),
                    core::mem::size_of::<u64>(),
                );
                Ok(())
            },
            _ => Err(NotNumericError),
        }
    }
}

impl std::fmt::Debug for RawItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("RawItem")
            .field("size", &self.size())
            .field("header", self.header())
            .field(
                "raw",
                &format!("{:02X?}", unsafe {
                    &std::slice::from_raw_parts(self.data, self.size())
                }),
            )
            .finish()
    }
}
