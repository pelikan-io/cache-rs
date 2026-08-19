//! A reserved item is an item which has been allocated but not yet linked
//! in the hashtable.

use crate::segments::WriterPin;
use crate::RawItem;
use crate::Value;
use core::num::NonZeroU32;

/// An item that has been allocated in a segment but is not yet defined or
/// linked in the hashtable. Holds a `WriterPin` so the backing segment cannot
/// be parsed by a drain/evict until this reservation is defined AND published
/// (the pin releases when the `ReservedItem` drops, after the hashtable op).
#[derive(Debug)]
pub(crate) struct ReservedItem {
    item: RawItem,
    seg: NonZeroU32,
    generation: u16,
    offset: usize,
    _pin: WriterPin,
}

impl ReservedItem {
    /// Create a `ReservedItem` from its parts, taking ownership of the writer pin.
    ///
    /// `generation` is the reserving segment's generation, read while the
    /// `WriterPin` was already held. It is captured here rather than re-read at
    /// publish time only for economy: the pin holds the segment out of the
    /// `-> Free` transitions that bump the generation, so the two reads are
    /// equal by construction.
    pub fn new(
        item: RawItem,
        seg: NonZeroU32,
        generation: u16,
        offset: usize,
        pin: WriterPin,
    ) -> Self {
        Self {
            item,
            seg,
            generation,
            offset,
            _pin: pin,
        }
    }

    /// Store the key, value, and optional data into the item
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        self.item.define(key, value, optional)
    }

    /// Get the `RawItem` that backs the `ReservedItem`
    pub fn item(&self) -> RawItem {
        self.item
    }

    /// Get the segment offset
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Get the segment id
    pub fn seg(&self) -> NonZeroU32 {
        self.seg
    }

    /// Get the incarnation generation of the segment this space was reserved
    /// in — the generation the item's location must be published under.
    pub fn generation(&self) -> u16 {
        self.generation
    }
}
