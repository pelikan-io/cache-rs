//! Read-only view of a stored cache item.

use crate::CuckooCacheError;
use keyvalue::{TinyItem, Value};

/// A read-only view of an item stored in the cuckoo cache.
pub struct Item {
    raw: TinyItem,
}

impl Item {
    pub(crate) fn new(raw: TinyItem) -> Self {
        Self { raw }
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
    /// Returns `u32::MAX` for items with no expiry.
    pub fn expire(&self) -> u32 {
        self.raw.expire()
    }

    /// Perform a wrapping addition on the value.
    pub fn wrapping_add(&mut self, rhs: u64) -> Result<(), CuckooCacheError> {
        self.raw
            .wrapping_add(rhs)
            .map_err(|_| CuckooCacheError::NotNumeric)
    }

    /// Perform a saturating subtraction on the value.
    pub fn saturating_sub(&mut self, rhs: u64) -> Result<(), CuckooCacheError> {
        self.raw
            .saturating_sub(rhs)
            .map_err(|_| CuckooCacheError::NotNumeric)
    }
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Item")
            .field("expire", &self.expire())
            .field("raw", &self.raw)
            .finish()
    }
}
