//! A ghost queue that stores fingerprints (hashes) of recently evicted items
//! from the small FIFO queue. Used to decide whether a newly inserted item
//! should go directly into the main FIFO queue.

use ahash::AHashSet;
use std::collections::VecDeque;

pub(crate) struct GhostQueue {
    queue: VecDeque<u64>,
    set: AHashSet<u64>,
    capacity: usize,
}

impl GhostQueue {
    /// Create a new ghost queue with the given capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            set: AHashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Check if the given hash is in the ghost queue
    #[inline]
    pub fn contains(&self, hash: u64) -> bool {
        self.set.contains(&hash)
    }

    /// Insert a hash into the ghost queue. If the queue is at capacity, the
    /// oldest entry is evicted.
    pub fn insert(&mut self, hash: u64) {
        if self.set.contains(&hash) {
            return;
        }
        // Evict oldest entries until we have room
        while self.set.len() >= self.capacity {
            if let Some(old) = self.queue.pop_front() {
                self.set.remove(&old);
            } else {
                break;
            }
        }
        self.queue.push_back(hash);
        self.set.insert(hash);
    }

    /// Remove a hash from the ghost queue (on ghost hit)
    pub fn remove(&mut self, hash: u64) {
        self.set.remove(&hash);
        // We leave the entry in the VecDeque; it will be cleaned up during
        // capacity management in insert()
    }

    /// Clear the ghost queue
    pub fn clear(&mut self) {
        self.queue.clear();
        self.set.clear();
    }
}
