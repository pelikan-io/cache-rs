//! A slab allocator for item storage. Each item is stored as a heap-allocated
//! byte buffer with associated metadata for the S3-FIFO eviction algorithm.

use crate::item::RawItem;

/// Which FIFO queue an item belongs to
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Queue {
    Small,
    Main,
}

/// Per-item data stored in the slab
pub(crate) struct ItemData {
    /// Raw item bytes: [ItemHeader][optional][key][value]
    pub(crate) data: Box<[u8]>,
    /// Cached hash of the key
    pub(crate) hash: u64,
    /// S3-FIFO frequency counter (0-3)
    pub(crate) freq: u8,
    /// Which FIFO queue this item belongs to
    pub(crate) queue: Queue,
    /// Expiry time in seconds since cache creation (0 = never expires)
    pub(crate) expire_at: u32,
    /// Tombstone marker for lazy cleanup from FIFO queues
    pub(crate) deleted: bool,
}

impl ItemData {
    /// Check if the raw item key matches the given key
    pub(crate) fn key_eq(&self, key: &[u8]) -> bool {
        let raw = RawItem::from_ptr(self.data.as_ptr() as *mut u8);
        raw.key() == key
    }
}

enum SlabEntry {
    Occupied(ItemData),
    Vacant { next_free: u32 },
}

/// A slab allocator that stores items in a Vec with free list management.
/// Items are referenced by their index (u32) in the slab.
pub(crate) struct Slab {
    entries: Vec<SlabEntry>,
    free_head: u32,
    len: u32,
}

/// Sentinel value indicating the end of the free list
const FREE_LIST_END: u32 = u32::MAX;

impl Slab {
    /// Create a new empty slab
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            free_head: FREE_LIST_END,
            len: 0,
        }
    }

    /// Allocate a new slab entry and return its index
    pub fn allocate(&mut self, size: usize, hash: u64, expire_at: u32, queue: Queue) -> u32 {
        let data = vec![0u8; size].into_boxed_slice();
        let item_data = ItemData {
            data,
            hash,
            freq: 0,
            queue,
            expire_at,
            deleted: false,
        };

        if self.free_head != FREE_LIST_END {
            let index = self.free_head;
            match &self.entries[index as usize] {
                SlabEntry::Vacant { next_free } => {
                    self.free_head = *next_free;
                }
                SlabEntry::Occupied(_) => panic!("free list corruption"),
            }
            self.entries[index as usize] = SlabEntry::Occupied(item_data);
            self.len += 1;
            index
        } else {
            let index = self.entries.len() as u32;
            self.entries.push(SlabEntry::Occupied(item_data));
            self.len += 1;
            index
        }
    }

    /// Free a slab entry, returning it to the free list
    pub fn free(&mut self, index: u32) {
        self.entries[index as usize] = SlabEntry::Vacant {
            next_free: self.free_head,
        };
        self.free_head = index;
        self.len -= 1;
    }

    /// Get an immutable reference to the item data at the given index
    pub fn get(&self, index: u32) -> Option<&ItemData> {
        match self.entries.get(index as usize) {
            Some(SlabEntry::Occupied(data)) => Some(data),
            _ => None,
        }
    }

    /// Get a mutable reference to the item data at the given index
    pub fn get_mut(&mut self, index: u32) -> Option<&mut ItemData> {
        match self.entries.get_mut(index as usize) {
            Some(SlabEntry::Occupied(data)) => Some(data),
            _ => None,
        }
    }

    /// Clear all entries
    pub fn clear(&mut self) {
        self.entries.clear();
        self.free_head = FREE_LIST_END;
        self.len = 0;
    }
}
