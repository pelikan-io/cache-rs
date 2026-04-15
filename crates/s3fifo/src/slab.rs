//! A pre-allocated arena-based slab allocator for item storage.
//!
//! Items are stored in a contiguous mmap'd byte buffer backed by
//! [`datatier::Memory`]. A size-class segregated free list provides O(1)
//! allocation and deallocation for variable-sized items without
//! per-item heap allocations.

use datatier::{Datapool, Memory};
use keyvalue::RawItem;

/// Which FIFO queue an item belongs to
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Queue {
    Small,
    Main,
}

/// Per-slot metadata stored in a parallel Vec, indexed by slot id.
pub(crate) struct SlotMeta {
    /// Byte offset into the arena where this slot's data begins
    pub(crate) offset: u32,
    /// Allocated size (the size-class size, >= requested size)
    pub(crate) alloc_size: u32,
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
    /// Size class index for O(1) return to the correct free list
    size_class: u8,
}

enum SlabEntry {
    Occupied(SlotMeta),
    Vacant { next_free: u32 },
}

/// Size classes for the arena allocator. Each class is roughly 1.5x the
/// previous, providing bounded internal fragmentation (~50% worst case,
/// much less in practice for 8-byte-aligned cache items).
const SIZE_CLASSES: &[u32] = &[
    8, 16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192,
    12288, 16384, 24576, 32768, 49152, 65536, 131072, 262144, 524288, 1048576,
];

/// Per-size-class free list head. Freed regions form an intrusive linked
/// list inside the arena buffer itself (each freed region stores a u32
/// next-pointer at its start).
struct SizeClassBin {
    free_head: u32,
}

/// Sentinel value for end-of-list
const NONE: u32 = u32::MAX;

/// Find the index of the smallest size class that can hold `size` bytes.
fn size_class_index(size: usize) -> usize {
    // Binary search for the first class >= size
    match SIZE_CLASSES.binary_search(&(size as u32)) {
        Ok(i) => i,
        Err(i) => i, // first class larger than size
    }
}

/// A pre-allocated arena-based slab allocator.
///
/// Items are stored in a contiguous byte buffer. Each item is identified
/// by a slot id (u32 index into the metadata Vec). The arena uses
/// size-class segregated free lists with a bump-pointer fallback for
/// fresh allocations.
pub(crate) struct Slab {
    /// The pre-allocated arena buffer, backed by datatier.
    /// Kept alive to own the mmap region; accessed via `arena_base`.
    #[allow(dead_code)]
    arena: Box<dyn Datapool>,
    /// Cached base pointer (stable because mmap does not move)
    arena_base: *mut u8,
    /// Total arena capacity in bytes
    arena_capacity: usize,
    /// Bump pointer: next fresh byte offset in the arena
    arena_top: u32,

    /// Metadata entries indexed by slot id
    entries: Vec<SlabEntry>,
    /// Free list of slot id indices (for recycling u32 handles)
    id_free_head: u32,

    /// Per-size-class free lists
    bins: Vec<SizeClassBin>,

    /// Number of occupied slots
    len: u32,
}

// SAFETY: The raw pointer `arena_base` points into the mmap region owned
// by `arena` (Box<dyn Datapool>). Datapool: Send, and the pointer is only
// used while `Slab` is alive and exclusively borrowed.
unsafe impl Send for Slab {}

impl Slab {
    /// Create a new slab with a pre-allocated arena of the given capacity.
    pub fn new(arena_capacity: usize) -> Result<Self, std::io::Error> {
        let mut arena: Box<dyn Datapool> = Box::new(Memory::create(arena_capacity)?);
        let arena_base = arena.as_mut_slice().as_mut_ptr();

        let bins = SIZE_CLASSES
            .iter()
            .map(|_| SizeClassBin { free_head: NONE })
            .collect();

        Ok(Self {
            arena,
            arena_base,
            arena_capacity,
            arena_top: 0,
            entries: Vec::new(),
            id_free_head: NONE,
            bins,
            len: 0,
        })
    }

    /// Allocate a slot for an item of the given size. Returns the slot id,
    /// or `None` if the arena is exhausted.
    pub fn allocate(
        &mut self,
        size: usize,
        hash: u64,
        expire_at: u32,
        queue: Queue,
    ) -> Option<u32> {
        let class_idx = size_class_index(size);

        // Try the target size class and larger classes for a free region
        let (offset, alloc_size, used_class) = self.find_free_region(class_idx, size)?;

        // Zero the region for RawItem::define
        unsafe {
            std::ptr::write_bytes(self.arena_base.add(offset as usize), 0, alloc_size as usize);
        }

        // Get or allocate a slot id
        let slot_id = self.alloc_slot_id();

        self.entries[slot_id as usize] = SlabEntry::Occupied(SlotMeta {
            offset,
            alloc_size,
            hash,
            freq: 0,
            queue,
            expire_at,
            deleted: false,
            size_class: used_class as u8,
        });
        self.len += 1;

        Some(slot_id)
    }

    /// Free a slot, returning its arena region to the appropriate size-class
    /// free list and its slot id to the id free list.
    pub fn free(&mut self, index: u32) {
        let (offset, size_class) = match &self.entries[index as usize] {
            SlabEntry::Occupied(meta) => (meta.offset, meta.size_class as usize),
            SlabEntry::Vacant { .. } => return,
        };

        // Push the region onto its size-class free list (intrusive: store
        // the next pointer at the start of the freed region)
        let old_head = self.bins[size_class].free_head;
        unsafe {
            let ptr = self.arena_base.add(offset as usize) as *mut u32;
            ptr.write(old_head);
        }
        self.bins[size_class].free_head = offset;

        // Return the slot id
        self.entries[index as usize] = SlabEntry::Vacant {
            next_free: self.id_free_head,
        };
        self.id_free_head = index;
        self.len -= 1;
    }

    /// Get an immutable reference to the slot metadata at the given index.
    pub fn get(&self, index: u32) -> Option<&SlotMeta> {
        match self.entries.get(index as usize) {
            Some(SlabEntry::Occupied(meta)) => Some(meta),
            _ => None,
        }
    }

    /// Get a mutable reference to the slot metadata at the given index.
    pub fn get_mut(&mut self, index: u32) -> Option<&mut SlotMeta> {
        match self.entries.get_mut(index as usize) {
            Some(SlabEntry::Occupied(meta)) => Some(meta),
            _ => None,
        }
    }

    /// Get a raw mutable pointer to the item data for the given slot.
    /// The pointer is valid for `alloc_size` bytes.
    pub fn data_ptr(&self, index: u32) -> Option<*mut u8> {
        match self.entries.get(index as usize) {
            Some(SlabEntry::Occupied(meta)) => {
                Some(unsafe { self.arena_base.add(meta.offset as usize) })
            }
            _ => None,
        }
    }

    /// Get the allocated size for the given slot.
    pub fn item_size(&self, index: u32) -> Option<usize> {
        match self.entries.get(index as usize) {
            Some(SlabEntry::Occupied(meta)) => Some(meta.alloc_size as usize),
            _ => None,
        }
    }

    /// Check if the slot is occupied, not deleted, and has the given key.
    /// Combines metadata check and key comparison in a single call to
    /// avoid double-borrow issues in callers.
    pub fn is_match(&self, index: u32, key: &[u8]) -> bool {
        match self.entries.get(index as usize) {
            Some(SlabEntry::Occupied(meta)) => {
                if meta.deleted {
                    return false;
                }
                let raw = RawItem::from_ptr(unsafe { self.arena_base.add(meta.offset as usize) });
                raw.key() == key
            }
            _ => false,
        }
    }

    /// Reset the arena. All slots are invalidated.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.id_free_head = NONE;
        self.arena_top = 0;
        for bin in &mut self.bins {
            bin.free_head = NONE;
        }
        self.len = 0;
    }

    // ── internal helpers ─────────────────────────────────────────────

    /// Try to find a free region starting from `class_idx`. Falls back to
    /// larger classes, then to bump allocation.
    fn find_free_region(
        &mut self,
        class_idx: usize,
        requested: usize,
    ) -> Option<(u32, u32, usize)> {
        // Try free lists from the target class upward
        for (idx, bin) in self.bins.iter_mut().enumerate().skip(class_idx) {
            if bin.free_head != NONE {
                let offset = bin.free_head;
                // Pop from free list: read next pointer from the region
                let next = unsafe {
                    let ptr = self.arena_base.add(offset as usize) as *const u32;
                    ptr.read()
                };
                bin.free_head = next;
                let alloc_size = SIZE_CLASSES[idx];
                return Some((offset, alloc_size, idx));
            }
        }

        // Bump allocate with the target size class
        let alloc_size = if class_idx < SIZE_CLASSES.len() {
            SIZE_CLASSES[class_idx]
        } else {
            // Item larger than largest class: use exact size (8-byte aligned)
            ((requested + 7) & !7) as u32
        };

        if (self.arena_top as usize) + (alloc_size as usize) <= self.arena_capacity {
            let offset = self.arena_top;
            self.arena_top += alloc_size;
            // For oversized items, use a class index beyond the table
            let used_class = class_idx.min(SIZE_CLASSES.len() - 1);
            return Some((offset, alloc_size, used_class));
        }

        None
    }

    /// Get or create a slot id.
    fn alloc_slot_id(&mut self) -> u32 {
        if self.id_free_head != NONE {
            let id = self.id_free_head;
            match &self.entries[id as usize] {
                SlabEntry::Vacant { next_free } => {
                    self.id_free_head = *next_free;
                }
                _ => panic!("slot id free list corruption"),
            }
            id
        } else {
            let id = self.entries.len() as u32;
            // Push a placeholder that will be overwritten by the caller
            self.entries.push(SlabEntry::Vacant { next_free: NONE });
            id
        }
    }
}
