//! Core datastructure

use crate::Value;
use crate::*;

use std::collections::VecDeque;

/// A cache using the S3-FIFO eviction algorithm. S3-FIFO uses a small FIFO
/// queue for probation, a CLOCK ring for the main cache, and a ghost queue
/// of recently evicted fingerprints for re-admission decisions.
pub struct S3Fifo {
    pub(crate) hashtable: HashTable,
    pub(crate) slab: Slab,
    pub(crate) small: VecDeque<u32>,
    pub(crate) main: Clock,
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
            let meta = self.slab.get(index)?;
            (meta.expire_at, meta.deleted)
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
        let meta = self.slab.get_mut(index).unwrap();
        if meta.freq < 3 {
            meta.freq += 1;
        }

        let raw = RawItem::from_ptr(self.slab.data_ptr(index).unwrap());
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
            let meta = self.slab.get(index)?;
            (meta.expire_at, meta.deleted)
        };

        if deleted {
            return None;
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.remove_item_internal(index, hash);
            return None;
        }

        let raw = RawItem::from_ptr(self.slab.data_ptr(index).unwrap());
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
        let index = self
            .slab
            .allocate(size, hash, expire_at, queue)
            .ok_or(S3FifoError::NoFreeSpace)?;

        // Write item data into the arena buffer
        {
            let ptr = self.slab.data_ptr(index).unwrap();
            let mut raw = RawItem::from_ptr(ptr);
            raw.define(key, value, optional);
        }

        // The actual allocated size (may be larger due to size-class rounding)
        let alloc_size = self.slab.item_size(index).unwrap();

        // Insert into hashtable
        match self.hashtable.insert(hash, index, key, &self.slab) {
            Ok(old) => {
                if let Some(old_index) = old {
                    let old_size = self.slab.item_size(old_index).unwrap_or(0);
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
            self.main.push(index);
        } else {
            self.small.push_back(index);
            self.small_bytes += alloc_size;
        }
        self.current_bytes += alloc_size;
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

        let _index = self
            .hashtable
            .get(key, hash, &self.slab)
            .ok_or(S3FifoError::NotFound)?;

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
                Some(meta) => (meta.alloc_size as usize, meta.queue),
                None => return false,
            };

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

        let ptr = self.slab.data_ptr(index).ok_or(S3FifoError::NotFound)?;
        let mut raw = RawItem::from_ptr(ptr);
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

        let ptr = self.slab.data_ptr(index).ok_or(S3FifoError::NotFound)?;
        let mut raw = RawItem::from_ptr(ptr);
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
            Some(meta) if !meta.deleted => (meta.alloc_size as usize, meta.queue),
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

    fn evict_one(&mut self) -> bool {
        let small_len = self.small.len();
        for _ in 0..small_len {
            match self.process_small_head() {
                ProcessResult::Freed => return true,
                ProcessResult::Moved => continue,
                ProcessResult::Empty => break,
            }
        }

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

    fn drain_small_one(&mut self) -> bool {
        matches!(
            self.process_small_head(),
            ProcessResult::Freed | ProcessResult::Moved
        )
    }

    fn process_small_head(&mut self) -> ProcessResult {
        let index = match self.small.pop_front() {
            Some(idx) => idx,
            None => return ProcessResult::Empty,
        };

        let (deleted, freq, hash, size, expire_at) = match self.slab.get(index) {
            Some(meta) => (
                meta.deleted,
                meta.freq,
                meta.hash,
                meta.alloc_size as usize,
                meta.expire_at,
            ),
            None => return ProcessResult::Freed,
        };

        if deleted {
            self.slab.free(index);
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
            let meta = self.slab.get_mut(index).unwrap();
            meta.freq = 0;
            meta.queue = Queue::Main;

            self.small_bytes -= size;
            self.main.push(index);

            #[cfg(feature = "metrics")]
            ITEM_PROMOTE.increment();

            ProcessResult::Moved
        } else {
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

    fn process_main_head(&mut self) -> ProcessResult {
        let index = match self.main.peek_and_advance() {
            Some(idx) => idx,
            None => return ProcessResult::Empty,
        };

        let (deleted, freq, hash, size, expire_at) = match self.slab.get(index) {
            Some(meta) => (
                meta.deleted,
                meta.freq,
                meta.hash,
                meta.alloc_size as usize,
                meta.expire_at,
            ),
            None => {
                self.main.remove_at_hand();
                return ProcessResult::Freed;
            }
        };

        if deleted {
            self.main.remove_at_hand();
            self.slab.free(index);
            return self.process_main_head();
        }

        let now = self.current_secs();
        if expire_at > 0 && now >= expire_at {
            self.main.remove_at_hand();
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EXPIRE.increment();

            return ProcessResult::Freed;
        }

        if freq > 0 {
            // CLOCK second chance: reset frequency, advance the hand.
            // The item stays in place — no data movement.
            let meta = self.slab.get_mut(index).unwrap();
            meta.freq = 0;

            #[cfg(feature = "metrics")]
            ITEM_REINSERT.increment();

            ProcessResult::Moved
        } else {
            self.main.remove_at_hand();
            self.hashtable.remove_by_index(hash, index);
            self.slab.free(index);
            self.current_bytes -= size;
            self.live_items -= 1;

            #[cfg(feature = "metrics")]
            ITEM_EVICT.increment();

            ProcessResult::Freed
        }
    }

    fn expire_queue_small(&mut self, now: u32) -> usize {
        let mut queue = std::mem::take(&mut self.small);
        let mut count = 0;
        let len = queue.len();

        for _ in 0..len {
            let index = queue.pop_front().unwrap();
            let (expired, hash, size, deleted) = match self.slab.get(index) {
                Some(meta) if !meta.deleted => (
                    meta.expire_at > 0 && now >= meta.expire_at,
                    meta.hash,
                    meta.alloc_size as usize,
                    false,
                ),
                _ => (false, 0, 0, true),
            };

            if deleted {
                self.slab.free(index);
            } else if expired {
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

        self.small = queue;
        count
    }

    fn expire_queue_main(&mut self, now: u32) -> usize {
        let mut count = 0;
        let scan_len = self.main.len();

        for _ in 0..scan_len {
            let index = match self.main.peek_and_advance() {
                Some(idx) => idx,
                None => break,
            };

            let (expired, hash, size, deleted) = match self.slab.get(index) {
                Some(meta) if !meta.deleted => (
                    meta.expire_at > 0 && now >= meta.expire_at,
                    meta.hash,
                    meta.alloc_size as usize,
                    false,
                ),
                _ => (false, 0, 0, true),
            };

            if deleted {
                self.main.remove_at_hand();
                self.slab.free(index);
            } else if expired {
                self.main.remove_at_hand();
                self.hashtable.remove_by_index(hash, index);
                self.slab.free(index);
                self.current_bytes -= size;
                self.live_items -= 1;
                count += 1;

                #[cfg(feature = "metrics")]
                ITEM_EXPIRE.increment();
            }
            // else: live item, hand already advanced past it
        }

        count
    }
}

enum ProcessResult {
    Freed,
    Moved,
    Empty,
}
