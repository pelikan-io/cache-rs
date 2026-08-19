//! A raw byte-level representation of an item.
//!
//! The [`RawItem`] provides direct byte-level access to item data stored as
//! a packed buffer of `[ItemHeader][optional][key][value]`.
//!
//! Numeric items (`Value::U64`) use an extended, 8-aligned value slot:
//! `[ItemHeader][optional][key][pad][value: u64][version: u64]`, where the
//! derived pad brings the value to an 8-byte boundary. Both words are
//! accessed atomically, and in-place updates may race each other AND
//! readers: without the `integrity` feature the value RMW is lock-free;
//! with `integrity` the version word doubles as a per-item seqlock writer
//! lock so the value and the item CRC change as one unit. The version also
//! feeds CAS-token construction: every in-place update bumps it by two, so
//! tokens observe increments (matching memcached, where incr/decr assign a
//! fresh cas unique).

use crate::item::*;
use crate::NotNumericError;
use crate::Value;
use core::sync::atomic::{fence, AtomicU64, Ordering};

/// The raw byte-level representation of an item.
///
/// This is a thin wrapper around a raw pointer to a packed item buffer.
/// The caller is responsible for ensuring the pointer is valid and properly
/// aligned.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawItem {
    data: *mut u8,
}

impl RawItem {
    /// Create a `RawItem` from a pointer.
    ///
    /// # Safety
    ///
    /// The pointer must point to a valid item buffer with a properly
    /// initialized [`ItemHeader`]. Undefined behavior results from
    /// passing an invalid or misaligned pointer. Numeric items
    /// additionally require the buffer to start 8-byte aligned (which
    /// segment placement guarantees) so that the value and version
    /// words are atomically addressable.
    pub fn from_ptr(ptr: *mut u8) -> RawItem {
        Self { data: ptr }
    }

    /// Get an immutable reference to the item's header.
    pub fn header(&self) -> &ItemHeader {
        unsafe { &*(self.data as *const ItemHeader) }
    }

    /// Get a mutable pointer to the item's header.
    fn header_mut(&mut self) -> *mut ItemHeader {
        self.data as *mut ItemHeader
    }

    /// Returns the key length.
    #[inline]
    pub fn klen(&self) -> u8 {
        self.header().klen()
    }

    /// Borrow the key bytes.
    pub fn key(&self) -> &[u8] {
        unsafe {
            let ptr = self.data.add(self.key_offset());
            let len = self.klen() as usize;
            std::slice::from_raw_parts(ptr, len)
        }
    }

    /// Returns the value length as stored in the header.
    #[inline]
    fn vlen(&self) -> u32 {
        self.header().vlen()
    }

    /// Atomic view of the numeric value word.
    ///
    /// # Safety
    ///
    /// Caller must ensure the item is numeric: the value slot is then
    /// 8-aligned by construction (aligned item start + derived pad).
    #[inline]
    unsafe fn value_word(&self) -> &AtomicU64 {
        &*(self.data.add(self.value_offset()) as *const AtomicU64)
    }

    /// Atomic view of the numeric version word (seqlock).
    ///
    /// # Safety
    ///
    /// Caller must ensure the item is numeric.
    #[inline]
    unsafe fn version_word(&self) -> &AtomicU64 {
        &*(self.data.add(self.value_offset() + 8) as *const AtomicU64)
    }

    /// Borrow the value, returning either bytes or a decoded u64.
    ///
    /// Numeric values are read with a seqlock so a concurrent in-place
    /// update can never be observed torn. Updates are word-atomic, so
    /// the value load itself cannot tear; the odd-check and version
    /// re-check are load-bearing for `integrity` builds, where they keep
    /// this read ordered against a writer's paired value+CRC update. In
    /// non-`integrity` builds writers never publish an odd version and a
    /// version change merely causes a harmless retry. Note that loom
    /// cannot verify seqlock orderings (no SC total order in its model,
    /// and these atomics are conjured from raw buffer pointers, which
    /// loom's types cannot model) — the protocol shape is pinned by
    /// concurrency tests instead.
    pub fn value(&self) -> Value<'_> {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked; slot aligned by construction.
            let (value_word, version_word) = unsafe { (self.value_word(), self.version_word()) };
            loop {
                let v1 = version_word.load(Ordering::Acquire);
                if v1 & 1 == 1 {
                    // write in progress
                    std::hint::spin_loop();
                    continue;
                }
                let value = value_word.load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                let v2 = version_word.load(Ordering::Relaxed);
                if v1 == v2 {
                    return Value::U64(value);
                }
            }
        } else {
            let bytes = unsafe {
                let ptr = self.data.add(self.value_offset());
                let len = self.vlen() as usize;
                std::slice::from_raw_parts(ptr, len)
            };
            Value::Bytes(bytes)
        }
    }

    /// Current seqlock version of a numeric item, for CAS-token
    /// construction. Every in-place update bumps this by two, so tokens
    /// built from it observe increments. A racing read (odd or stale
    /// version) only produces a token that is already stale — a spurious
    /// CAS failure, the safe direction.
    #[inline]
    pub fn numeric_version(&self) -> Option<u64> {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked.
            Some(unsafe { self.version_word() }.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    /// Returns the optional data length.
    #[inline]
    pub fn olen(&self) -> u8 {
        self.header().olen()
    }

    /// Borrow the optional data, if any.
    pub fn optional(&self) -> Option<&[u8]> {
        let olen = self.olen() as usize;
        if olen > 0 {
            unsafe {
                let ptr = self.data.add(self.optional_offset());
                Some(std::slice::from_raw_parts(ptr, olen))
            }
        } else {
            None
        }
    }

    /// Check the header magic bytes.
    #[inline]
    pub fn check_magic(&self) {
        self.header().check_magic()
    }

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.header().is_deleted()
    }

    pub fn set_deleted(&mut self, deleted: bool) {
        unsafe { (*self.header_mut()).set_deleted(deleted) }
    }

    /// Write key, value, and optional data into the item buffer.
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        unsafe {
            (*self.header_mut()).init();
            (*self.header_mut()).set_olen(optional.len() as u8);
            (*self.header_mut()).set_klen(key.len() as u8);

            // Copy optional data
            std::ptr::copy_nonoverlapping(
                optional.as_ptr(),
                self.data.add(self.optional_offset()),
                optional.len(),
            );

            // Copy key
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.data.add(self.key_offset()),
                key.len(),
            );

            // Copy value
            match value {
                Value::Bytes(v) => {
                    (*self.header_mut()).set_numeric(false);
                    (*self.header_mut()).set_vlen(v.len() as u32);
                    std::ptr::copy_nonoverlapping(
                        v.as_ptr(),
                        self.data.add(self.value_offset()),
                        v.len(),
                    );
                }
                Value::U64(v) => {
                    (*self.header_mut()).set_numeric(true);
                    (*self.header_mut()).set_vlen(8);

                    // Zero the derived alignment pad between the key and
                    // the value slot (deterministic bytes for the CRC).
                    let pad = numeric_value_pad(key.len(), optional.len());
                    if pad > 0 {
                        std::ptr::write_bytes(self.data.add(self.key_offset() + key.len()), 0, pad);
                    }

                    // The item is not yet published, so plain-vs-atomic
                    // ordering is moot; use atomic stores for uniformity
                    // with the seqlock protocol. Native-endian.
                    self.value_word().store(v, Ordering::Relaxed);
                    self.version_word().store(0, Ordering::Relaxed);
                }
            }

            // Compute and store the CRC32.
            #[cfg(feature = "integrity")]
            {
                let crc = self.compute_crc();
                (*self.header_mut()).set_crc32(crc);
            }
        }
    }

    /// Wrapping in-place addition on a numeric value, returning the new
    /// value.
    ///
    /// The read-modify-write is atomic with respect to OTHER WRITERS:
    /// callers may race `fetch_wrapping_add`/`fetch_saturating_sub` on
    /// the same item from multiple threads (the engine above pins the
    /// segment only as a reader, so writers are NOT serialized
    /// externally) and no update is lost. Every update also bumps the
    /// item's version by exactly two, staling outstanding CAS tokens.
    ///
    /// Without the `integrity` feature the update is lock-free: the
    /// value word RMW is a native atomic (wrapping is `fetch_add`'s
    /// overflow behavior on `AtomicU64`). With `integrity`, the update
    /// additionally recomputes the stored CRC, and value + CRC must
    /// change as one unit; the version word then acts as a per-item
    /// writer lock (odd = write in progress), which keeps
    /// [`Self::check_integrity`] exact under concurrency — it never
    /// misreports a healthy racing update as corruption and always
    /// detects real value corruption.
    pub fn fetch_wrapping_add(&self, rhs: u64) -> Result<u64, NotNumericError> {
        #[cfg(feature = "integrity")]
        return self.locked_numeric_update(|v| v.wrapping_add(rhs));

        #[cfg(not(feature = "integrity"))]
        {
            let (value_word, version_word) = self.numeric_words()?;
            // Wait-free: wrapping on overflow is fetch_add's native
            // behavior. AcqRel chains each writer's RMW with its
            // neighbors on the value word; the returned value is exact
            // for this call even under contention.
            let new = value_word
                .fetch_add(rhs, Ordering::AcqRel)
                .wrapping_add(rhs);
            version_word.fetch_add(2, Ordering::Release);
            Ok(new)
        }
    }

    /// Saturating in-place subtraction on a numeric value, returning the
    /// new value. See [`Self::fetch_wrapping_add`] for the concurrency
    /// contract.
    pub fn fetch_saturating_sub(&self, rhs: u64) -> Result<u64, NotNumericError> {
        #[cfg(feature = "integrity")]
        return self.locked_numeric_update(|v| v.saturating_sub(rhs));

        #[cfg(not(feature = "integrity"))]
        {
            let (value_word, version_word) = self.numeric_words()?;
            // Lock-free CAS loop: saturation has no native fetch_ op.
            let prev = value_word
                .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(rhs))
                })
                .expect("closure never returns None");
            version_word.fetch_add(2, Ordering::Release);
            Ok(prev.saturating_sub(rhs))
        }
    }

    /// Both numeric atomic words, or `NotNumericError`.
    #[cfg(not(feature = "integrity"))]
    #[inline]
    fn numeric_words(&self) -> Result<(&AtomicU64, &AtomicU64), NotNumericError> {
        if !self.header().is_numeric() {
            return Err(NotNumericError);
        }
        // SAFETY: is_numeric checked; slot aligned by construction.
        Ok(unsafe { (self.value_word(), self.version_word()) })
    }

    /// Serialized numeric update for `integrity` builds: value and CRC
    /// are two separate words that must change together, so concurrent
    /// writers take a per-item spin lock on the version word (CAS the
    /// even version to odd). This is the classic seqlock writer side —
    /// readers already treat an odd version as write-in-progress — and
    /// it doubles as the mutual exclusion that makes the RMW atomic.
    ///
    /// Ordering: the successful lock CAS is `Acquire`, pairing with the
    /// previous writer's `Release` unlock, so this writer observes the
    /// prior value and CRC. A `Release` fence sits between the odd
    /// version store and the data stores (Boehm's seqlock writer): a
    /// reader that reads-from one of the data stores and then executes
    /// its `Acquire` fence is thereby guaranteed to observe the odd
    /// version on its re-check and retry — without the fence, a reader
    /// could formally pair a new value with a stale CRC while both
    /// version loads still returned the old even value. The unlock
    /// store is `Release`: any reader whose `Acquire` load of the
    /// version returns the new even value is guaranteed to observe the
    /// matching value and CRC.
    #[cfg(feature = "integrity")]
    fn locked_numeric_update(&self, op: impl Fn(u64) -> u64) -> Result<u64, NotNumericError> {
        Ok(self.lock_numeric_version()?.update(op))
    }

    /// Acquire the numeric item's seqlock writer lock WITHOUT modifying
    /// the value — a publish gate for engines layered above.
    ///
    /// While the returned guard is alive, every in-place numeric writer
    /// (`fetch_wrapping_add`/`fetch_saturating_sub`, which serialize on
    /// the same version word in `integrity` builds) is excluded, and the
    /// version reported by [`NumericVersionGuard::version`] — the even
    /// value observed at lock time — cannot advance. This lets a caller
    /// atomically pair "the version is still V" with a publish action of
    /// its own (e.g. a hashtable slot swap that supersedes this item, or
    /// a relocation that byte-copies the item and relinks its location —
    /// see [`NumericVersionGuard::stamp_relocated_copy`]): any concurrent
    /// increment either completes before the lock (the caller sees its
    /// bumped version / final value) or starts after the guard drops (and
    /// can then observe whatever the caller published).
    ///
    /// The guard resolves one of two ways: [`NumericVersionGuard::update`]
    /// applies a value update under the lock and unlocks at version + 2
    /// (the normal writer protocol — `fetch_wrapping_add` and friends are
    /// built on it); dropping it without an update restores the same even
    /// version, so "the version advances by exactly two per update" stays
    /// true and concurrent seqlock readers resume with an identical
    /// value/version pair. Hold it only across short, lock-free sections
    /// (readers and writers spin while it is held).
    ///
    /// Only meaningful under `integrity` (where numeric writers take
    /// the version-word lock); without that feature writers are
    /// lock-free fetch-ops that would ignore this gate, so the API is
    /// not offered.
    #[cfg(feature = "integrity")]
    pub fn lock_numeric_version(&self) -> Result<NumericVersionGuard<'_>, NotNumericError> {
        if !self.header().is_numeric() {
            return Err(NotNumericError);
        }
        // SAFETY: is_numeric checked; slot aligned by construction.
        let version_word = unsafe { self.version_word() };

        // Lock: transition the version from even to odd (the same
        // protocol as `locked_numeric_update`; Acquire pairs with the
        // previous writer's Release unlock).
        let mut v = version_word.load(Ordering::Relaxed);
        loop {
            if v & 1 == 1 {
                // another writer holds the lock
                std::hint::spin_loop();
                v = version_word.load(Ordering::Relaxed);
                continue;
            }
            match version_word.compare_exchange_weak(
                v,
                v.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => v = observed,
            }
        }
        Ok(NumericVersionGuard {
            raw: self,
            version: v,
        })
    }

    /// Atomic view of the header CRC field.
    ///
    /// The header is `repr(C, packed)`, so this goes through pointer
    /// arithmetic (the CRC is the trailing 4 bytes of the header, at
    /// item offset 8 — 4-aligned given 8-aligned item starts), never a
    /// field reference.
    #[cfg(feature = "integrity")]
    #[inline]
    fn crc_word(&self) -> &core::sync::atomic::AtomicU32 {
        unsafe { &*(self.data.add(ITEM_HDR_SIZE - 4) as *const core::sync::atomic::AtomicU32) }
    }

    /// Verify the item's CRC32 integrity.
    ///
    /// Returns `true` if the stored CRC matches a freshly computed one.
    /// Numeric items are checked under the seqlock so a concurrent
    /// in-place update is never misreported as corruption.
    #[cfg(feature = "integrity")]
    pub fn check_integrity(&self) -> bool {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked.
            let version_word = unsafe { self.version_word() };
            loop {
                let v1 = version_word.load(Ordering::Acquire);
                if v1 & 1 == 1 {
                    std::hint::spin_loop();
                    continue;
                }
                let value = unsafe { self.value_word() }.load(Ordering::Relaxed);
                let stored = self.crc_word().load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                let v2 = version_word.load(Ordering::Relaxed);
                if v1 == v2 {
                    return stored == self.compute_crc_numeric(value);
                }
            }
        } else {
            self.header().crc32() == self.compute_crc()
        }
    }

    /// Compute CRC32 over the item with the CRC field zeroed.
    ///
    /// For numeric items this covers the header, optional, key, pad, and
    /// the value word — but NOT the version word, which is seqlock
    /// protocol state (corrupting it can only cause spurious CAS-token
    /// mismatches, never silent data corruption).
    #[cfg(feature = "integrity")]
    fn compute_crc(&self) -> u32 {
        if self.header().is_numeric() {
            let value = unsafe { self.value_word() }.load(Ordering::Relaxed);
            self.compute_crc_numeric(value)
        } else {
            self.compute_crc_span(self.value_offset() + self.vlen() as usize)
        }
    }

    /// Numeric CRC: hash up to the value slot from the buffer, then the
    /// value from a caller-supplied snapshot (an atomic load), so the
    /// computation never does a plain read of the concurrently-updated
    /// word.
    #[cfg(feature = "integrity")]
    fn compute_crc_numeric(&self, value: u64) -> u32 {
        let crc_field_size = std::mem::size_of::<u32>();
        let crc_field_offset = ITEM_HDR_SIZE - crc_field_size;

        let mut hasher = crc32fast::Hasher::new();
        unsafe {
            // header before the CRC field
            hasher.update(std::slice::from_raw_parts(self.data, crc_field_offset));
            // CRC field treated as zeros
            hasher.update(&[0u8; 4]);
            // optional + key + pad (immutable after define)
            let after_offset = crc_field_offset + crc_field_size;
            let value_offset = self.value_offset();
            if value_offset > after_offset {
                hasher.update(std::slice::from_raw_parts(
                    self.data.add(after_offset),
                    value_offset - after_offset,
                ));
            }
        }
        // the value word, from the snapshot (native-endian bytes)
        hasher.update(&value.to_ne_bytes());
        hasher.finalize()
    }

    /// Bytes-item CRC over `[0, end)` with the CRC field zeroed.
    #[cfg(feature = "integrity")]
    fn compute_crc_span(&self, end: usize) -> u32 {
        let crc_field_size = std::mem::size_of::<u32>();
        let crc_field_offset = ITEM_HDR_SIZE - crc_field_size;

        let mut hasher = crc32fast::Hasher::new();
        unsafe {
            let before = std::slice::from_raw_parts(self.data, crc_field_offset);
            hasher.update(before);
            hasher.update(&[0u8; 4]);
            let after_offset = crc_field_offset + crc_field_size;
            if end > after_offset {
                let after =
                    std::slice::from_raw_parts(self.data.add(after_offset), end - after_offset);
                hasher.update(after);
            }
        }
        hasher.finalize()
    }

    // -- Offset calculations --

    #[inline]
    fn optional_offset(&self) -> usize {
        ITEM_HDR_SIZE
    }

    #[inline]
    fn key_offset(&self) -> usize {
        self.optional_offset() + self.olen() as usize
    }

    #[inline]
    fn value_offset(&self) -> usize {
        let unpadded = self.key_offset() + self.klen() as usize;
        if self.header().is_numeric() {
            unpadded + numeric_value_pad(self.klen() as usize, self.olen() as usize)
        } else {
            unpadded
        }
    }

    /// Returns item size, rounded up to 8-byte alignment. Numeric items
    /// include the alignment pad and the seqlock version word.
    pub fn size(&self) -> usize {
        let klen = self.klen() as usize;
        let olen = self.olen() as usize;
        let extra = if self.header().is_numeric() {
            numeric_value_pad(klen, olen) + 8
        } else {
            0
        };
        let raw = ITEM_HDR_SIZE + olen + klen + extra + self.vlen() as usize;
        ((raw >> 3) + 1) << 3
    }
}

/// RAII guard for a numeric item's seqlock writer lock — see
/// [`RawItem::lock_numeric_version`]. While alive it excludes in-place
/// numeric writers and freezes the observed version. It resolves one of
/// two ways:
///
/// - [`NumericVersionGuard::update`] applies a value update under the
///   lock (the seqlock writer protocol: value + CRC as one unit) and
///   unlocks two above the observed version;
/// - dropping it without an update restores the same even version
///   (nothing changed).
#[cfg(feature = "integrity")]
pub struct NumericVersionGuard<'a> {
    raw: &'a RawItem,
    version: u64,
}

#[cfg(feature = "integrity")]
impl NumericVersionGuard<'_> {
    /// The item's seqlock version observed when the lock was taken —
    /// always even, and frozen while this guard is alive.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Stamp a relocated byte copy of this guard's item with the frozen
    /// EVEN version.
    ///
    /// A raw byte copy of a numeric item taken while this guard is held
    /// necessarily captures the source's version word in its LOCKED (odd)
    /// state — the lock acquisition itself made it odd. Publishing that
    /// copy as-is would create a permanently write-in-progress item that
    /// wedges every seqlock reader and writer forever. The value and CRC
    /// bytes in the copy ARE coherent (this guard excludes all in-place
    /// writers), so storing the guard's observed even version makes the
    /// copy exactly the value/version pair the guard froze.
    ///
    /// `copy` must be a byte copy of this guard's item (checked: it must
    /// at least be numeric) that is not yet reachable by any other
    /// thread; the store is `Relaxed` because the caller's subsequent
    /// publish (e.g. a Release hashtable CAS) is what orders it for
    /// readers of the new location.
    pub fn stamp_relocated_copy(&self, copy: &RawItem) {
        assert!(
            copy.header().is_numeric(),
            "stamp_relocated_copy requires a numeric item copy"
        );
        // SAFETY: is_numeric checked; a byte copy of an aligned numeric
        // item placed at an 8-aligned start keeps the words aligned.
        unsafe { copy.version_word() }.store(self.version, Ordering::Relaxed);
    }

    /// Apply `op` to the value under this lock, consuming the guard.
    /// Stores the new value and its CRC as one seqlocked unit, then
    /// unlocks at `version() + 2`. Returns the new value.
    ///
    /// Any validation performed between taking the lock and calling this
    /// is atomic with the update with respect to every other party that
    /// serializes on this item's version word (in-place numeric writers,
    /// and cas publishes that re-verify tokens under the lock).
    pub fn update(self, op: impl FnOnce(u64) -> u64) -> u64 {
        // SAFETY: lock_numeric_version checked is_numeric; slot aligned
        // by construction.
        let (value_word, version_word) =
            unsafe { (self.raw.value_word(), self.raw.version_word()) };

        // Order the odd (write-in-progress) version store — done by the
        // lock acquisition — before the data stores for the seqlock
        // readers (Boehm's seqlock writer; see `lock_numeric_version`).
        fence(Ordering::Release);

        let new = op(value_word.load(Ordering::Relaxed));
        value_word.store(new, Ordering::Relaxed);
        let crc = self.raw.compute_crc_numeric(new);
        self.raw.crc_word().store(crc, Ordering::Relaxed);

        // Unlock: back to even, two above the pre-update version. The
        // guard's Drop (which would restore the OLD version) must not
        // run.
        version_word.store(self.version.wrapping_add(2), Ordering::Release);
        core::mem::forget(self);
        new
    }
}

#[cfg(feature = "integrity")]
impl Drop for NumericVersionGuard<'_> {
    fn drop(&mut self) {
        // Unlock by restoring the pre-lock even version (no update was
        // applied). Release pairs with the next lock's Acquire CAS:
        // everything the guard holder did (e.g. a hashtable publish) is
        // visible to the next numeric writer before it applies its
        // update.
        // SAFETY: lock_numeric_version checked is_numeric.
        unsafe { self.raw.version_word() }.store(self.version, Ordering::Release);
    }
}

impl std::fmt::Debug for RawItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("RawItem")
            .field("size", &self.size())
            .field("header", self.header())
            .field(
                "raw",
                &format!("{:02X?}", unsafe {
                    &std::slice::from_raw_parts(self.data, self.size())
                }),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8-aligned scratch buffer, as segment placement guarantees.
    fn aligned_buf(len_words: usize) -> Vec<u64> {
        vec![0u64; len_words]
    }

    fn define_numeric(buf: &mut [u64], key: &[u8], value: u64, optional: &[u8]) -> RawItem {
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(key, Value::U64(value), optional);
        raw
    }

    #[test]
    fn numeric_slot_alignment_sweep() {
        let mut buf = aligned_buf(128);
        let base = buf.as_ptr() as usize;
        let key = [0xAAu8; 255];
        let opt = [0xBBu8; 63];

        for klen in 1..=255usize {
            for olen in [0usize, 1, 7, 63] {
                let raw = define_numeric(&mut buf, &key[..klen], 42, &opt[..olen]);
                let value_addr = base + raw.value_offset();
                assert_eq!(
                    value_addr % 8,
                    0,
                    "value misaligned for klen={klen} olen={olen}"
                );
                assert_eq!((value_addr + 8) % 8, 0);
                assert_eq!(raw.value(), Value::U64(42));
                // size helper and instance size agree
                assert_eq!(raw.size(), item_size(klen, &Value::U64(42), olen));
            }
        }
    }

    #[test]
    fn bytes_items_unpadded() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"value"), b"");
        // bytes layout is exactly header + key + value
        assert_eq!(raw.size(), item_size(3, &Value::Bytes(b"value"), 0));
        assert_eq!(raw.size(), (((ITEM_HDR_SIZE + 3 + 5) >> 3) + 1) << 3);
        assert_eq!(raw.value(), Value::Bytes(b"value"));
    }

    #[test]
    fn seqlocked_ops() {
        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 5, b"");

        assert_eq!(raw.numeric_version(), Some(0));

        // each op bumps the version by exactly two (odd transient state)
        assert_eq!(raw.fetch_wrapping_add(1), Ok(6));
        assert_eq!(raw.numeric_version(), Some(2));
        assert_eq!(raw.value(), Value::U64(6));

        assert_eq!(raw.fetch_saturating_sub(2), Ok(4));
        assert_eq!(raw.numeric_version(), Some(4));

        // wrap at the 64-bit mark (memcached incr semantics)
        assert_eq!(raw.fetch_wrapping_add(u64::MAX - 3), Ok(0));

        // saturate at zero (memcached decr semantics)
        assert_eq!(raw.fetch_saturating_sub(100), Ok(0));
    }

    #[test]
    fn non_numeric_ops_error() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"text"), b"");
        assert_eq!(raw.fetch_wrapping_add(1), Err(NotNumericError));
        assert_eq!(raw.fetch_saturating_sub(1), Err(NotNumericError));
        assert_eq!(raw.numeric_version(), None);
    }

    #[test]
    fn pad_bytes_zeroed() {
        let mut buf = aligned_buf(64);
        // pollute the buffer first
        for w in buf.iter_mut() {
            *w = u64::MAX;
        }
        let raw = define_numeric(&mut buf, b"k", 1, b"");
        let pad = numeric_value_pad(1, 0);
        if pad > 0 {
            let start = raw.key_offset() + 1;
            let bytes = unsafe { std::slice::from_raw_parts(raw.data.add(start), pad) };
            assert!(bytes.iter().all(|&b| b == 0), "pad not zeroed: {bytes:?}");
        }
    }

    #[cfg(feature = "integrity")]
    #[test]
    fn crc_covers_numeric_value_across_increments() {
        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 5, b"opt");
        assert!(raw.check_integrity());

        // the CRC is updated under the seqlock on every increment
        raw.fetch_wrapping_add(1).unwrap();
        assert!(raw.check_integrity());
        raw.fetch_saturating_sub(2).unwrap();
        assert!(raw.check_integrity());

        // corrupting the VALUE is detected (full coverage — the
        // requirement that forced the seqlock design)
        let value_off = raw.value_offset();
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(!raw.check_integrity());
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(raw.check_integrity());

        // corrupting key or optional is detected
        let key_off = raw.key_offset();
        unsafe { *raw.data.add(key_off) ^= 0xFF };
        assert!(!raw.check_integrity());
        unsafe { *raw.data.add(key_off) ^= 0xFF };
        assert!(raw.check_integrity());

        // the version word is protocol state, excluded from coverage:
        // corrupting it can only cause spurious CAS-token mismatches
        let version_off = raw.value_offset() + 8;
        unsafe { *raw.data.add(version_off) ^= 0x02 };
        assert!(raw.check_integrity());
    }

    /// Concurrent increments on one item must not lose updates: the
    /// value RMW has to be atomic, not load/op/store. Before the fix,
    /// N threads x M `fetch_wrapping_add(1)` ended short of N*M.
    #[test]
    fn concurrent_wrapping_add_loses_no_updates() {
        const THREADS: usize = 8;
        const OPS: usize = 10_000;

        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 0, b"");
        // RawItem is a raw pointer and deliberately not Send/Sync; the
        // test shares the (valid for the scope) address as usize, the
        // same way concurrent engine threads alias one item buffer.
        let addr = raw.data as usize;

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(move || {
                    let raw = RawItem::from_ptr(addr as *mut u8);
                    for _ in 0..OPS {
                        raw.fetch_wrapping_add(1).unwrap();
                    }
                });
            }
        });

        let expected = (THREADS * OPS) as u64;
        assert_eq!(
            raw.value(),
            Value::U64(expected),
            "lost updates: expected {expected}"
        );
        // every update bumps the version by exactly 2, never leaving it odd
        assert_eq!(raw.numeric_version(), Some(2 * expected));
    }

    /// Concurrent decrements: with the initial value equal to the total
    /// number of subtractions, any lost update leaves the counter above
    /// zero. A second phase then verifies the floor under contention.
    #[test]
    fn concurrent_saturating_sub_loses_no_updates_and_floors_at_zero() {
        const THREADS: usize = 8;
        const OPS: usize = 10_000;
        let total = (THREADS * OPS) as u64;

        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", total, b"");
        let addr = raw.data as usize;

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(move || {
                    let raw = RawItem::from_ptr(addr as *mut u8);
                    for _ in 0..OPS {
                        raw.fetch_saturating_sub(1).unwrap();
                    }
                });
            }
        });
        assert_eq!(raw.value(), Value::U64(0), "lost updates: expected 0");
        assert_eq!(raw.numeric_version(), Some(2 * total));

        // floor: many concurrent subs against a small value saturate at 0
        raw.fetch_wrapping_add(3).unwrap();
        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(move || {
                    let raw = RawItem::from_ptr(addr as *mut u8);
                    for _ in 0..OPS {
                        raw.fetch_saturating_sub(2).unwrap();
                    }
                });
            }
        });
        assert_eq!(raw.value(), Value::U64(0));
        assert_eq!(raw.numeric_version(), Some(2 * (2 * total + 1)));
    }

    /// The stored CRC must stay consistent with the value under
    /// concurrent writers and concurrent readers: `check_integrity`
    /// may never report corruption on a healthy item. Before the fix,
    /// overlapped writers broke the odd-version write-in-progress
    /// invariant, so a reader could pair a value with a stale CRC.
    #[cfg(feature = "integrity")]
    #[test]
    fn integrity_holds_under_concurrent_updates() {
        use std::sync::atomic::AtomicBool;

        const WRITERS: usize = 4;
        const OPS: usize = 20_000;

        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 0, b"");
        let addr = raw.data as usize;
        let done = AtomicBool::new(false);
        let done = &done;

        std::thread::scope(|s| {
            let checker = s.spawn(move || {
                let raw = RawItem::from_ptr(addr as *mut u8);
                let mut checks = 0u64;
                while !done.load(Ordering::Acquire) {
                    assert!(raw.check_integrity(), "false corruption report");
                    checks += 1;
                }
                checks
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|_| {
                    s.spawn(move || {
                        let raw = RawItem::from_ptr(addr as *mut u8);
                        for _ in 0..OPS {
                            raw.fetch_wrapping_add(1).unwrap();
                        }
                    })
                })
                .collect();
            for w in writers {
                w.join().unwrap();
            }
            done.store(true, Ordering::Release);
            let checks = checker.join().unwrap();
            assert!(checks > 0, "checker never ran concurrently");
        });
        assert_eq!(raw.value(), Value::U64((WRITERS * OPS) as u64));
        assert!(raw.check_integrity());
    }

    #[cfg(feature = "integrity")]
    #[test]
    fn crc_covers_bytes_value() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"value"), b"");
        assert!(raw.check_integrity());
        let value_off = raw.value_offset();
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(!raw.check_integrity());
    }
}
