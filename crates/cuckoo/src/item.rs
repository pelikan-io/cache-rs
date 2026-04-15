//! Read-only view of a stored cache item.

use crate::CuckooCacheError;
use keyvalue::{RawItem, Value};

/// A read-only view of an item stored in the cuckoo cache.
pub struct Item {
    raw: RawItem,
    expire: u32,
}

impl Item {
    pub(crate) fn new(raw: RawItem, expire: u32) -> Self {
        Self { raw, expire }
    }

    /// Borrow the item's key.
    pub fn key(&self) -> &[u8] {
        self.raw.key()
    }

    /// Borrow the item's value.
    pub fn value(&self) -> Value<'_> {
        self.raw.value()
    }

    /// The item's expiration timestamp as seconds since cache creation.
    /// Returns 0 for items with no expiry.
    pub fn expire(&self) -> u32 {
        self.expire
    }

    /// Borrow the optional data.
    pub fn optional(&self) -> Option<&[u8]> {
        self.raw.optional()
    }

    /// Perform a wrapping addition on the value. Returns an error if the item
    /// is not a numeric type.
    pub fn wrapping_add(&mut self, rhs: u64) -> Result<(), CuckooCacheError> {
        self.raw
            .wrapping_add(rhs)
            .map_err(|_| CuckooCacheError::NotNumeric)
    }

    /// Perform a saturating subtraction on the value. Returns an error if the
    /// item is not a numeric type.
    pub fn saturating_sub(&mut self, rhs: u64) -> Result<(), CuckooCacheError> {
        self.raw
            .saturating_sub(rhs)
            .map_err(|_| CuckooCacheError::NotNumeric)
    }
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Item")
            .field("expire", &self.expire)
            .field("raw", &self.raw)
            .finish()
    }
}
