//! Items are the base unit of data stored within the cache.

mod reserved;

use crate::segments::SegmentGuard;
use keyvalue::{RawItem, Value};

pub(crate) use reserved::ReservedItem;

/// The base unit of data returned by a cache lookup.
///
/// An `Item` pins the segment it points into: while it is alive, that
/// segment cannot be recycled, merged, or compacted, so the key and
/// value bytes it exposes remain stable.
///
/// An `Item` borrows the [`Segcache`](crate::Segcache) it came from, so the
/// cache cannot be dropped or moved while the item is alive. Using the item
/// after the cache is dropped is rejected:
///
/// ```compile_fail,E0505
/// use segcache::Segcache;
/// use std::time::Duration;
///
/// let cache = Segcache::builder().build().unwrap();
/// cache.insert(b"key", b"value", None, Duration::from_secs(60)).unwrap();
/// let item = cache.get(b"key").unwrap();
/// drop(cache);
/// let _ = item.value();
/// ```
///
/// So is dropping the item after the cache:
///
/// ```compile_fail,E0505
/// use segcache::Segcache;
/// use std::time::Duration;
///
/// let cache = Segcache::builder().build().unwrap();
/// cache.insert(b"key", b"value", None, Duration::from_secs(60)).unwrap();
/// let _item = cache.get(b"key").unwrap();
/// drop(cache);
/// ```
pub struct Item<'a> {
    cas: u64,
    raw: RawItem,
    _guard: SegmentGuard<'a>,
}

impl<'a> Item<'a> {
    /// `raw` must be the item `acquire_item_at` returned together with
    /// `guard`.
    pub(crate) fn new(raw: RawItem, cas: u64, guard: SegmentGuard<'a>) -> Self {
        Item {
            cas,
            raw,
            _guard: guard,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn check_magic(&self) {
        self.raw.check_magic()
    }

    /// Borrow the item key
    pub fn key(&self) -> &[u8] {
        self.raw.key()
    }

    /// Borrow the item value
    pub fn value(&self) -> Value<'_> {
        self.raw.value()
    }

    /// CAS value for the item, matching the memcache protocol's 64-bit
    /// "cas unique". Combines the item's location with its segment's
    /// generation counter; any update, relocation, or segment recycle
    /// invalidates outstanding values.
    pub fn cas(&self) -> u64 {
        self.cas
    }

    /// Borrow the optional data
    pub fn optional(&self) -> Option<&[u8]> {
        self.raw.optional()
    }

    /// Returns true if the item has been soft-deleted.
    pub fn is_deleted(&self) -> bool {
        self.raw.is_deleted()
    }
}

impl std::fmt::Debug for Item<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Item")
            .field("cas", &self.cas())
            .field("raw", &self.raw)
            .finish()
    }
}
