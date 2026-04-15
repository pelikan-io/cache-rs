//! Core datastructure

use crate::Value;
use crate::*;

use std::collections::VecDeque;

/// A cache using the S3-FIFO eviction algorithm. S3-FIFO uses three FIFO
/// queues — small, main, and ghost — to efficiently separate one-hit wonders
/// from frequently accessed items. New items enter the small queue. Items
/// accessed again are promoted to the main queue. A ghost queue of recently
/// evicted key fingerprints enables fast re-admission.
pub struct S3Fifo {
    pub(crate) hashtable: HashTable,
    pub(crate) slab: Slab,
    pub(crate) small: VecDeque<u32>,
    pub(crate) main: VecDeque<u32>,
    pub(crate) ghost: GhostQueue,
    pub(crate) heap_size: usize,
    pub(crate) small_quota: usize,
    pub(crate) current_bytes: usize,
    pub(crate) small_bytes: usize,
    pub(crate) live_items: usize,
    pub(crate) started: Instant,
}

impl S3Fifo {
    /// Returns a new `Builder` which is used to configure and construct an
    /// `S3Fifo` instance.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// let cache = S3Fifo::builder()
    ///     .heap_size(64 * MB)
    ///     .hash_power(16)
    ///     .build()
    ///     .expect("failed to create cache");
    /// ```
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Gets a count of items in the cache. This is an inexpensive O(1)
    /// operation.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// let cache = S3Fifo::builder().build().expect("failed to create cache");
    /// assert_eq!(cache.items(), 0);
    /// ```
    pub fn items(&self) -> usize {
        self.live_items
    }

    /// Get the item with the provided key. On a hit, the item's frequency
    /// counter is incremented (capped at 3).
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    /// use std::time::Duration;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    /// assert!(cache.get(b"coffee").is_none());
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// let item = cache.get(b"coffee").expect("didn't get item back");
    /// assert_eq!(item.value(), b"strong");
    /// ```
    pub fn get(&mut self, key: &[u8]) -> Option<Item> {
        let hash = self.hashtable.hash(key);
        let index = self.hashtable.get(key, hash, &self.slab)?;

        let (expire_at, deleted) = {
            let entry = self.slab.get(index)?;
            (entry.expire_at, entry.deleted)
        };

        if deleted {
            return None;
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.remove_item_internal(index, hash);

            #[cfg(feature = "metrics")]
            ITEM_EXPIRE.increment();

            return None;
        }

        // Increment frequency (cap at 3)
        let entry = self.slab.get_mut(index).unwrap();
        if entry.freq < 3 {
            entry.freq += 1;
        }

        let raw = RawItem::from_ptr(entry.data.as_mut_ptr());
        let cas = self.hashtable.get_cas(hash);

        let item = Item::new(raw, cas);
        item.check_magic();

        Some(item)
    }

    /// Get the item with the provided key without increasing the item
    /// frequency. Useful for combined operations that check for presence.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    /// assert!(cache.get_no_freq_incr(b"coffee").is_none());
    /// ```
    pub fn get_no_freq_incr(&mut self, key: &[u8]) -> Option<Item> {
        let hash = self.hashtable.hash(key);
        let index = self.hashtable.get(key, hash, &self.slab)?;

        let (expire_at, deleted) = {
            let entry = self.slab.get(index)?;
            (entry.expire_at, entry.deleted)
        };

        if deleted {
            return None;
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.remove_item_internal(index, hash);
            return None;
        }

        let entry = self.slab.get_mut(index).unwrap();
        let raw = RawItem::from_ptr(entry.data.as_mut_ptr());
        let cas = self.hashtable.get_cas(hash);

        let item = Item::new(raw, cas);
        item.check_magic();

        Some(item)
    }

    /// Insert a new item into the cache. May return an error indicating that
    /// the insert was not successful.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    /// use std::time::Duration;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    /// assert!(cache.get(b"drink").is_none());
    ///
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"coffee");
    ///
    /// cache.insert(b"drink", b"whisky", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"whisky");
    /// ```
    pub fn insert<'a, T: Into<Value<'a>>>(
        &mut self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
    ) -> Result<(), S3FifoError> {
        let value: Value = value.into();
        let optional = optional.unwrap_or(&[]);

        // Calculate aligned item size (same formula as segcache)
        let size = (((ITEM_HDR_SIZE + key.len() + size_of(&value) + optional.len()) >> 3) + 1) << 3;

        // Check if item is too large
        if size > self.heap_size {
            return Err(S3FifoError::ItemOversized { size });
        }

        let hash = self.hashtable.hash(key);

        // Remove existing entry for this key if present
        if let Some(old_index) = self.hashtable.get(key, hash, &self.slab) {
            self.remove_item_internal(old_index, hash);

            #[cfg(feature = "metrics")]
            ITEM_REPLACE.increment();
        }

        // Check ghost queue: if the key was recently evicted from small,
        // insert directly into main
        let use_main = self.ghost.contains(hash);
        if use_main {
            self.ghost.remove(hash);

            #[cfg(feature = "metrics")]
            GHOST_HIT.increment();
        }

        // Ensure total capacity
        self.ensure_capacity(size)?;

        // If inserting into small, ensure small has room
        if !use_main {
            self.ensure_small_capacity(size);
        }

        // Calculate expiry
        let ttl_secs = std::cmp::min(u32::MAX as u64, ttl.as_secs()) as u32;
        let expire_at = if ttl_secs == 0 {
            0
        } else {
            self.current_secs().saturating_add(ttl_secs)
        };

        let queue = if use_main { Queue::Main } else { Queue::Small };
        let index = self.slab.allocate(size, hash, expire_at, queue);

        // Write item data into the slab buffer
        {
            let entry = self.slab.get_mut(index).unwrap();
            let mut raw = RawItem::from_ptr(entry.data.as_mut_ptr());
            raw.define(key, value, optional);
        }

        // Insert into hashtable
        match self.hashtable.insert(hash, index, key, &self.slab) {
            Ok(old) => {
                if let Some(old_index) = old {
                    // Replaced an existing entry (shouldn't normally happen
                    // since we deleted above, but handle gracefully)
                    let old_size = self.slab.get(old_index).map(|e| e.data.len()).unwrap_or(0);
                    self.slab.free(old_index);
                    self.current_bytes -= old_size;
                    self.live_items -= 1;
                }
            }
            Err(()) => {
                self.slab.free(index);
                return Err(S3FifoError::HashTableInsertEx);
            }
        }

        // Add to the appropriate FIFO queue
        if use_main {
            self.main.push_back(index);
        } else {
            self.small.push_back(index);
            self.small_bytes += size;
        }
        self.current_bytes += size;
        self.live_items += 1;

        #[cfg(feature = "metrics")]
        {
            ITEM_INSERT.increment();
            ITEM_CURRENT.set(self.live_items as _);
            ITEM_CURRENT_BYTES.set(self.current_bytes as _);
        }

        Ok(())
    }

    /// Performs a CAS operation, inserting the item only if the CAS value
    /// matches the current value for that item.
    ///
    /// ```
    /// use s3fifo::{S3Fifo, S3FifoError};
    /// use std::time::Duration;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    ///
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(S3FifoError::NotFound)
    /// );
    ///
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(S3FifoError::Exists)
    /// );
    ///
    /// let current = cache.get(b"drink").expect("not found");
    /// assert!(cache.cas(b"drink", b"whisky", None, Duration::ZERO, current.cas()).is_ok());
    /// let item = cache.get(b"drink").expect("not found");
    /// assert_eq!(item.value(), b"whisky");
    /// ```
    pub fn cas<'a, T: Into<Value<'a>>>(
        &mut self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
        cas: u32,
    ) -> Result<(), S3FifoError> {
        let hash = self.hashtable.hash(key);

        // Check that the item exists
        let _index = self
            .hashtable
            .get(key, hash, &self.slab)
            .ok_or(S3FifoError::NotFound)?;

        // Check CAS value
        let bucket_cas = self.hashtable.get_cas(hash);
        if cas != bucket_cas {
            return Err(S3FifoError::Exists);
        }

        self.insert(key, value, optional, ttl)
    }

    /// Remove the item with the given key, returns a bool indicating if it was
    /// removed.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    /// use std::time::Duration;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    /// assert_eq!(cache.delete(b"coffee"), false);
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// assert!(cache.get(b"coffee").is_some());
    /// assert_eq!(cache.delete(b"coffee"), true);
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    pub fn delete(&mut self, key: &[u8]) -> bool {
        let hash = self.hashtable.hash(key);

        if let Some(index) = self.hashtable.delete(key, hash, &self.slab) {
            let (size, queue) = match self.slab.get(index) {
                Some(e) => (e.data.len(), e.queue),
                None => return false,
            };

            // Mark as deleted (tombstone) for lazy cleanup from queues
            self.slab.get_mut(index).unwrap().deleted = true;

            self.current_bytes -= size;
            if queue == Queue::Small {
                self.small_bytes -= size;
            }
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            {
                ITEM_DELETE.increment();
                ITEM_CURRENT.set(self.live_items as _);
                ITEM_CURRENT_BYTES.set(self.current_bytes as _);
            }

            true
        } else {
            false
        }
    }

    /// Scan and remove expired items from both queues, returning the number
    /// of items expired.
    ///
    /// ```
    /// use s3fifo::S3Fifo;
    /// use std::time::Duration;
    ///
    /// let mut cache = S3Fifo::builder().build().expect("failed to create cache");
    /// cache.insert(b"coffee", b"strong", None, Duration::from_secs(5));
    /// assert!(cache.get(b"coffee").is_some());
    ///
    /// std::thread::sleep(Duration::from_secs(6));
    /// cache.expire();
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    pub fn expire(&mut self) -> usize {
        let now = self.current_secs();
        let mut count = 0;
        count += self.expire_queue_small(now);
        count += self.expire_queue_main(now);
        count
    }

    /// Clear all items from the cache, returning the number of items cleared.
    pub fn clear(&mut self) -> usize {
        let count = self.live_items;

        self.small.clear();
        self.main.clear();
        self.slab.clear();
        self.hashtable.clear();
        self.ghost.clear();
        self.current_bytes = 0;
        self.small_bytes = 0;
        self.live_items = 0;

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.set(0);
            ITEM_CURRENT_BYTES.set(0);
        }

        count
    }

    /// Perform a wrapping addition on the value stored at the supplied key.
    pub fn wrapping_add(&mut self, key: &[u8], rhs: u64) -> Result<Item, S3FifoError> {
        let hash = self.hashtable.hash(key);
        let index = self
            .hashtable
            .get(key, hash, &self.slab)
            .ok_or(S3FifoError::NotFound)?;

        let entry = self.slab.get_mut(index).ok_or(S3FifoError::NotFound)?;
        let mut raw = RawItem::from_ptr(entry.data.as_mut_ptr());
        raw.wrapping_add(rhs).map_err(|_| S3FifoError::NotNumeric)?;

        let cas = self.hashtable.get_cas(hash);
        Ok(Item::new(raw, cas))
    }

    /// Perform a saturating subtraction on the value stored at the supplied
    /// key.
    pub fn saturating_sub(&mut self, key: &[u8], rhs: u64) -> Result<Item, S3FifoError> {
        let hash = self.hashtable.hash(key);
        let index = self
            .hashtable
            .get(key, hash, &self.slab)
            .ok_or(S3FifoError::NotFound)?;

        let entry = self.slab.get_mut(index).ok_or(S3FifoError::NotFound)?;
        let mut raw = RawItem::from_ptr(entry.data.as_mut_ptr());
        raw.saturating_sub(rhs)
            .map_err(|_| S3FifoError::NotNumeric)?;

        let cas = self.hashtable.get_cas(hash);
        Ok(Item::new(raw, cas))
    }

    // ── internal helpers ─────────────────────────────────────────────

    fn current_secs(&self) -> u32 {
        (Instant::now() - self.started).as_secs()
    }

    /// Remove an item from the hashtable and mark it deleted in the slab.
    fn remove_item_internal(&mut self, index: u32, hash: u64) {
        let (size, queue) = match self.slab.get(index) {
            Some(e) if !e.deleted => (e.data.len(), e.queue),
            _ => return,
        };

        self.hashtable.remove_by_index(hash, index);
        self.slab.get_mut(index).unwrap().deleted = true;

        self.current_bytes -= size;
        if queue == Queue::Small {
            self.small_bytes -= size;
        }
        self.live_items -= 1;

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.set(self.live_items as _);
            ITEM_CURRENT_BYTES.set(self.current_bytes as _);
        }
    }

    /// Evict items until we have room for `needed` bytes in total.
    fn ensure_capacity(&mut self, needed: usize) -> Result<(), S3FifoError> {
        while self.current_bytes + needed > self.heap_size {
            if !self.evict_one() {
                return Err(S3FifoError::NoFreeSpace);
            }
        }
        Ok(())
    }

    /// Drain items from the small queue until small_bytes has room for
    /// `needed` bytes. Items with freq > 0 are promoted to main.
    fn ensure_small_capacity(&mut self, needed: usize) {
        while self.small_bytes + needed > self.small_quota {
            if !self.drain_small_one() {
                break;
            }
        }
    }

    /// Try to evict one item from the cache, freeing bytes. Tries small
    /// first (evicting items with freq == 0, promoting those with freq > 0),
    /// then main.
    fn evict_one(&mut self) -> bool {
        // Try small: scan until we find an item with freq == 0 to evict
        let small_len = self.small.len();
        for _ in 0..small_len {
            match self.process_small_head() {
                ProcessResult::Freed => return true,
                ProcessResult::Moved => continue,
                ProcessResult::Empty => break,
            }
        }

        // Small exhausted or all promoted; try main
        let main_len = self.main.len();
        for _ in 0..main_len {
            match self.process_main_head() {
                ProcessResult::Freed => return true,
                ProcessResult::Moved => continue,
                ProcessResult::Empty => break,
            }
        }

        false
    }

    /// Drain one non-tombstone item from the small queue. Returns true if an
    /// item was processed (promoted or evicted).
    fn drain_small_one(&mut self) -> bool {
        matches!(
            self.process_small_head(),
            ProcessResult::Freed | ProcessResult::Moved
        )
    }

    /// Pop the head of the small queue and process it according to S3-FIFO:
    /// - tombstone/expired → free and report Freed
    /// - freq > 0 → promote to main, report Moved
    /// - freq == 0 → evict, add to ghost, report Freed
    fn process_small_head(&mut self) -> ProcessResult {
        let index = match self.small.pop_front() {
            Some(idx) => idx,
            None => return ProcessResult::Empty,
        };

        let (deleted, freq, hash, size, expire_at) = match self.slab.get(index) {
            Some(entry) => (
                entry.deleted,
                entry.freq,
                entry.hash,
                entry.data.len(),
                entry.expire_at,
            ),
            None => return ProcessResult::Freed, // shouldn't happen
        };

        if deleted {
            self.slab.free(index);
            // Recurse to get the next real item
            return self.process_small_head();
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.small_bytes -= size;
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EXPIRE.increment();

            return ProcessResult::Freed;
        }

        if freq > 0 {
            // Promote to main, reset frequency
            let entry = self.slab.get_mut(index).unwrap();
            entry.freq = 0;
            entry.queue = Queue::Main;

            self.small_bytes -= size;
            self.main.push_back(index);

            #[cfg(feature = "metrics")]
            ITEM_PROMOTE.increment();

            ProcessResult::Moved
        } else {
            // Evict: add fingerprint to ghost, remove from cache
            self.ghost.insert(hash);
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.small_bytes -= size;
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EVICT.increment();

            ProcessResult::Freed
        }
    }

    /// Pop the head of the main queue and process it:
    /// - tombstone/expired → free and report Freed
    /// - freq > 0 → reinsert at tail, report Moved
    /// - freq == 0 → evict, report Freed
    fn process_main_head(&mut self) -> ProcessResult {
        let index = match self.main.pop_front() {
            Some(idx) => idx,
            None => return ProcessResult::Empty,
        };

        let (deleted, freq, hash, size, expire_at) = match self.slab.get(index) {
            Some(entry) => (
                entry.deleted,
                entry.freq,
                entry.hash,
                entry.data.len(),
                entry.expire_at,
            ),
            None => return ProcessResult::Freed,
        };

        if deleted {
            self.slab.free(index);
            return self.process_main_head();
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EXPIRE.increment();

            return ProcessResult::Freed;
        }

        if freq > 0 {
            // Reinsert at tail of main, reset frequency
            let entry = self.slab.get_mut(index).unwrap();
            entry.freq = 0;

            self.main.push_back(index);

            #[cfg(feature = "metrics")]
            ITEM_REINSERT.increment();

            ProcessResult::Moved
        } else {
            // Evict
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EVICT.increment();

            ProcessResult::Freed
        }
    }

    /// Expire items in the small queue. We rotate through the queue, removing
    /// expired and tombstoned items, and putting live items back.
    fn expire_queue_small(&mut self, now: u32) -> usize {
        let mut queue = std::mem::take(&mut self.small);
        let mut count = 0;
        let len = queue.len();

        for _ in 0..len {
            let index = queue.pop_front().unwrap();
            match self.slab.get(index) {
                Some(entry) if !entry.deleted => {
                    if entry.expire_at > 0 && now >= entry.expire_at {
                        let hash = entry.hash;
                        let size = entry.data.len();
                        self.hashtable.remove_by_index(hash, index);
                        self.slab.free(index);
                        self.small_bytes -= size;
                        self.current_bytes -= size;
                        self.live_items -= 1;
                        count += 1;

                        #[cfg(feature = "metrics")]
                        ITEM_EXPIRE.increment();
                    } else {
                        queue.push_back(index);
                    }
                }
                _ => {
                    self.slab.free(index);
                }
            }
        }

        self.small = queue;
        count
    }

    /// Expire items in the main queue.
    fn expire_queue_main(&mut self, now: u32) -> usize {
        let mut queue = std::mem::take(&mut self.main);
        let mut count = 0;
        let len = queue.len();

        for _ in 0..len {
            let index = queue.pop_front().unwrap();
            match self.slab.get(index) {
                Some(entry) if !entry.deleted => {
                    if entry.expire_at > 0 && now >= entry.expire_at {
                        let hash = entry.hash;
                        let size = entry.data.len();
                        self.hashtable.remove_by_index(hash, index);
                        self.slab.free(index);
                        self.current_bytes -= size;
                        self.live_items -= 1;
                        count += 1;

                        #[cfg(feature = "metrics")]
                        ITEM_EXPIRE.increment();
                    } else {
                        queue.push_back(index);
                    }
                }
                _ => {
                    self.slab.free(index);
                }
            }
        }

        self.main = queue;
        count
    }
}

enum ProcessResult {
    /// Item was evicted or expired, freeing total bytes
    Freed,
    /// Item was promoted or reinserted (no total bytes freed)
    Moved,
    /// Queue is empty
    Empty,
}
