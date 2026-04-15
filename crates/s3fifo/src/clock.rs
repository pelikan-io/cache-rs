//! A fixed-capacity circular buffer implementing the CLOCK algorithm.
//!
//! Items are stored in a ring. A hand pointer advances through the ring.
//! "Reinsertion" is just advancing the hand — zero data movement.

/// A CLOCK ring buffer of `u32` slot ids.
pub(crate) struct Clock {
    ring: Vec<u32>,
    /// Number of live entries in the ring
    len: usize,
    /// Current hand position
    hand: usize,
}

/// Sentinel for an empty slot in the ring
const EMPTY: u32 = u32::MAX;

impl Clock {
    pub fn new() -> Self {
        Self {
            ring: Vec::new(),
            len: 0,
            hand: 0,
        }
    }

    /// Insert an item into the ring.
    pub fn push(&mut self, index: u32) {
        // Find a slot: try appending first, otherwise find an EMPTY slot
        // near the tail
        self.ring.push(index);
        self.len += 1;
    }

    /// Advance the hand and return the current item without removing it.
    /// Skips EMPTY slots. Returns None if the ring is empty.
    pub fn peek_and_advance(&mut self) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let ring_len = self.ring.len();
        for _ in 0..ring_len {
            let pos = self.hand % ring_len;
            self.hand = pos + 1;
            let val = self.ring[pos];
            if val != EMPTY {
                return Some(val);
            }
        }
        None
    }

    /// Remove the item at the current hand position (the one just returned
    /// by `peek_and_advance`). The hand has already advanced past it, so
    /// we mark the previous position as EMPTY.
    pub fn remove_at_hand(&mut self) {
        if self.ring.is_empty() {
            return;
        }
        let ring_len = self.ring.len();
        // hand was advanced past the item, so the item is at hand - 1
        let pos = (self.hand + ring_len - 1) % ring_len;
        if self.ring[pos] != EMPTY {
            self.ring[pos] = EMPTY;
            self.len -= 1;
        }
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Clear the ring.
    pub fn clear(&mut self) {
        self.ring.clear();
        self.len = 0;
        self.hand = 0;
    }
}
