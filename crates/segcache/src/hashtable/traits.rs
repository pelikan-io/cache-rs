//! Hashtable trait and key verification for cache operations.

use crate::hashtable::location::Location;
use crate::hashtable::table::SlotRef;

/// Outcome of comparing a key against the location published in a bucket slot.
///
/// The three arms exist because a slot's location can be *stale* — read from
/// the slot word a moment before a drain recycled the segment underneath it —
/// and the verifier is the only party that can tell the difference between
/// "this slot holds someone else's key" and "I could not look".
///
/// # Why `DifferentKey` is authoritative here and was not before
///
/// The pre-#91 verifier compared segment bytes with no pin and no generation
/// tag, so a `false` could mean either "different key" or "the bytes stopped
/// being this entry's while I was reading them". Distinguishing the two cost a
/// re-read of the slot word (`classify_failed_verify`) and an invariant
/// argument spanning both files.
///
/// A verifier that pins first cannot produce the second meaning: it either
/// gets a pin whose generation tag matches the location's — in which case the
/// bytes are published-immutable for as long as the pin is held, so the
/// compare is exact — or it gets no pin at all, which is [`Self::Unknown`].
pub(crate) enum Verified<P> {
    /// The location holds `key`, and the storage naming it is PINNED: `P` is
    /// that pin. Holding it is what makes the compare a plain read of
    /// immutable bytes rather than a race, so the pin is handed to the caller
    /// rather than dropped — a read path keeps it, a write path drops it.
    Match(P),
    /// The location holds a different key. Authoritative: nothing was
    /// mutating the compared bytes.
    DifferentKey,
    /// The location could not be pinned, so nothing was compared. Carries the
    /// location because triaging *why* needs it: a non-readable segment (a
    /// drain owns it — transient, and the key may well still be live) reads
    /// differently from a stale incarnation (a genuine miss). See
    /// `Segcache::triage_unknown_location`.
    Unknown(Location),
}

/// Trait for verifying that a key exists at a location.
///
/// The hashtable calls this during lookup/insert to confirm that a tag match
/// corresponds to an actual key match (avoiding false positives from hash
/// collisions in the 12-bit tag).
///
/// # Thread Safety
///
/// Implementations must be thread-safe (`Send + Sync`) as verification may be
/// called concurrently from multiple threads.
pub(crate) trait KeyVerifier: Send + Sync {
    /// The pin a successful verify hands back, proving the compared bytes
    /// were immutable while they were compared. Dropping it releases the
    /// storage; the hashtable never inspects it.
    type Pin;

    /// Verify that `key` exists at `location`, under a pin.
    fn verify(&self, key: &[u8], location: Location, allow_deleted: bool) -> Verified<Self::Pin>;

    /// Prefetch memory at the given location.
    ///
    /// Called by the hashtable after a tag match but before full verification.
    /// This allows overlapping memory prefetch with the Acquire barrier overhead.
    #[inline]
    fn prefetch(&self, _location: Location) {}
}

/// Outcome of a hashtable lookup.
///
/// The third arm is the read-path face of [`Verified::Unknown`]: the scan
/// found no match, but at least one candidate slot could not be verified, so
/// "absent" would be a guess. Reporting it instead of `Absent` is what keeps a
/// drain window from reading as a miss.
///
/// A scan NEVER waits or spins on `Unknown` — it remembers it and keeps
/// scanning, and only reports it if the scan ends with no match (#54: no
/// waiting inside the hashtable). What to do about it is decided at the
/// caller, which is the party holding the pins.
pub(crate) enum Lookup<T> {
    /// The key resolved.
    Found(T),
    /// The key is not in the table. Authoritative: every candidate slot was
    /// verified.
    Absent,
    /// No candidate matched, but at least one could not be verified — this
    /// location. The key may or may not be live.
    Unknown(Location),
}

impl<T> Lookup<T> {
    /// The `Found` payload, discarding the distinction between `Absent` and
    /// `Unknown`. Test-only: production read/write paths owe an unverifiable
    /// candidate different behaviour from a confirmed miss, so every one of
    /// them matches on all three arms.
    #[cfg(test)]
    pub(crate) fn found(self) -> Option<T> {
        match self {
            Lookup::Found(v) => Some(v),
            Lookup::Absent | Lookup::Unknown(_) => None,
        }
    }

    /// Whether the key resolved. Same caveat as [`Self::found`]: it collapses
    /// `Absent` and `Unknown`, which is why it is test-only — production code
    /// owes the two answers different behaviour.
    #[cfg(test)]
    pub(crate) fn is_found(&self) -> bool {
        matches!(self, Lookup::Found(_))
    }
}

/// A live entry a lookup resolved, together with everything the caller needs
/// to act on it without probing again.
///
/// `slot` is the load-bearing addition: it is what lets a reader re-read *the
/// slot word it found the entry in* and ask "is this entry still published?"
/// without a second full lookup. See `Segcache::get_pinned`.
pub(crate) struct Hit<P> {
    /// Where the item lives.
    pub(crate) location: Location,
    /// The slot the entry was found in — for the same-slot freshness compare.
    pub(crate) slot: SlotRef,
    /// The verifier's pin on `location`'s storage.
    pub(crate) pin: P,
}

/// Outcome of [`Hashtable::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Insert {
    /// A new entry (or a ghost resurrection) was created for the key.
    Created,
    /// An existing live entry was replaced; this was its location.
    Replaced(Location),
    /// A candidate slot could not be verified, so whether the key already has
    /// an entry is unknown — and publishing a fresh entry on a guess is how
    /// duplicates get created (#46). The caller must NOT treat this as
    /// "absent": roll back and restart.
    Unknown(Location),
}

/// What a relocation does to the entry's frequency.
///
/// Moving an item is not an access, so a relocation normally carries its
/// frequency over. S3-FIFO's main-queue second chance is the exception: the
/// item keeps its place by spending a step of it, so one that is not read
/// again falls to zero and is dropped on a later pass. Carrying the
/// frequency over there made a second chance permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelinkFreq {
    Preserve,
    Decrement,
}

/// Core trait for hashtable operations.
///
/// A hashtable maps keys to `Location` values, tracking the physical
/// location of items in storage. It also maintains frequency counters
/// for each item, supporting eviction algorithms like S3-FIFO.
///
/// # Ghost Entries
///
/// When an item is evicted, its hashtable entry can be converted to a "ghost"
/// entry. Ghosts preserve the frequency counter but mark the location as invalid
/// (`Location::GHOST`). When re-inserting a previously evicted key, the ghost's
/// frequency can be preserved, giving "second chance" semantics.
#[allow(dead_code)]
pub(crate) trait Hashtable: Send + Sync {
    /// Look up a key and return its location, frequency, slot and pin.
    ///
    /// This also increments the frequency counter (probabilistically for
    /// values > 16 using the ASFC algorithm).
    fn lookup<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<Hit<V::Pin>>;

    /// Look up a key without updating frequency.
    fn lookup_no_freq_update<V: KeyVerifier>(
        &self,
        key: &[u8],
        verifier: &V,
    ) -> Lookup<Hit<V::Pin>>;

    /// Check if a key exists without updating frequency.
    fn contains<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<()>;

    /// Insert or update a key's location.
    ///
    /// If the key already exists (live or ghost), updates the location and
    /// preserves the frequency. For ghosts, this "resurrects" the entry.
    ///
    /// # Returns
    /// - `Ok(Insert::Replaced(old))` if an existing entry was replaced
    /// - `Ok(Insert::Created)` if this was a new entry or ghost resurrection
    /// - `Ok(Insert::Unknown(loc))` if a candidate slot was unverifiable
    /// - `Err(())` if the hashtable is full
    fn insert<V: KeyVerifier>(
        &self,
        key: &[u8],
        location: Location,
        verifier: &V,
    ) -> Result<Insert, ()>;

    /// Remove a key from the hashtable.
    ///
    /// The entry must match the expected location (for ABA safety).
    fn remove(&self, key: &[u8], expected: Location) -> bool;

    /// Convert an entry to a ghost (preserves frequency).
    fn convert_to_ghost(&self, key: &[u8], expected: Location) -> bool;

    /// Update an item's location atomically.
    ///
    /// Used during compaction and tier migration. The entry must match
    /// the expected old location for the update to succeed.
    fn cas_location(
        &self,
        key: &[u8],
        old_location: Location,
        new_location: Location,
        freq: RelinkFreq,
    ) -> bool;

    /// Get the frequency of an item by key.
    fn get_frequency<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<u8>;

    /// Get the frequency of an item at a specific location.
    fn get_item_frequency(&self, key: &[u8], location: Location) -> Option<u8>;

    /// Get the frequency of a ghost entry.
    fn get_ghost_frequency(&self, key: &[u8]) -> Option<u8>;

    /// Clear all entries from the hashtable.
    fn clear(&self);
}
