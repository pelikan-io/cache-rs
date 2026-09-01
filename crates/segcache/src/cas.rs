//! CAS (Compare-And-Swap) token for memcached CAS operations.
//!
//! The CAS token uniquely identifies a specific version of an item in the
//! cache. It combines the item's location with the segment's generation
//! counter to prevent ABA problems when segments are reused.
//!
//! # Layout
//!
//! ```text
//! +---------------------------+------------------+
//! |          43..0            |      59..44      |
//! |         location          |    generation    |
//! |          44 bits          |     16 bits      |
//! +---------------------------+------------------+
//! ```
//!
//! The token is 60 bits total, fitting within a u64.

use crate::hashtable::Location;
use std::fmt;

/// A CAS token combining location and generation for ABA-safe versioning.
///
/// The token uniquely identifies a specific version of an item:
/// - **Location (44 bits)**: Identifies where the item is stored (segment + offset)
/// - **Generation (16 bits)**: Segment generation counter, incremented on reuse
///
/// When an item is updated, it gets a new location and/or generation, causing
/// CAS operations with the old token to fail. This prevents the ABA problem
/// where an item is deleted and a new item is written to the same location.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CasToken(u64);

impl CasToken {
    /// Mask for the 44-bit location portion.
    pub(crate) const LOCATION_MASK: u64 = 0xFFF_FFFF_FFFF;

    /// Shift for the 16-bit generation portion.
    const GENERATION_SHIFT: u32 = 44;

    /// Create a new CAS token from location and generation.
    #[inline]
    pub fn new(location: Location, generation: u16) -> Self {
        let raw = location.as_raw() | ((generation as u64) << Self::GENERATION_SHIFT);
        Self(raw)
    }

    /// Get the raw 60-bit value.
    #[inline]
    pub fn as_raw(&self) -> u64 {
        self.0
    }

    /// Construct from a raw 60-bit value.
    #[inline]
    #[allow(dead_code)]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Extract the location portion.
    #[inline]
    pub fn location(&self) -> Location {
        Location::from_raw(self.0 & Self::LOCATION_MASK)
    }

    /// Extract the generation portion.
    #[inline]
    pub fn generation(&self) -> u16 {
        (self.0 >> Self::GENERATION_SHIFT) as u16
    }
}

/// Fold a numeric item's seqlock version into its CAS token.
///
/// The multiplicative spread (odd constant, bijective over u64) keeps
/// distinct versions from colliding on low bits; the same (location,
/// generation, version) triple always produces the same token, and any
/// in-place update changes the version — so tokens observe increments,
/// matching memcached's do_add_delta assigning a fresh cas unique.
#[inline]
pub(crate) fn mix_version(raw: u64, version: u64) -> u64 {
    raw ^ version.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

impl fmt::Debug for CasToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CasToken(loc={:?}, gen={})",
            self.location(),
            self.generation()
        )
    }
}

impl fmt::Display for CasToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(all(test, not(model_checking)))]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_extract() {
        let loc = Location::new(0x123_4567_89AB);
        let generation = 0x1234;
        let token = CasToken::new(loc, generation);

        assert_eq!(token.location(), loc);
        assert_eq!(token.generation(), generation);
    }

    #[test]
    fn test_from_raw_roundtrip() {
        let loc = Location::new(0xABC_DEF0_1234);
        let generation = 0xFFFF;
        let token = CasToken::new(loc, generation);

        let raw = token.as_raw();
        let restored = CasToken::from_raw(raw);

        assert_eq!(restored.location(), loc);
        assert_eq!(restored.generation(), generation);
    }

    #[test]
    fn test_zero_generation() {
        let loc = Location::new(0x100);
        let token = CasToken::new(loc, 0);

        assert_eq!(token.generation(), 0);
        assert_eq!(token.location(), loc);
    }

    #[test]
    fn test_max_generation() {
        let loc = Location::new(0x100);
        let token = CasToken::new(loc, u16::MAX);

        assert_eq!(token.generation(), u16::MAX);
        assert_eq!(token.location(), loc);
    }

    #[test]
    fn test_max_location() {
        let loc = Location::new(Location::MAX_RAW);
        let generation = 0x5678;
        let token = CasToken::new(loc, generation);

        assert_eq!(token.location().as_raw(), Location::MAX_RAW);
        assert_eq!(token.generation(), generation);
    }

    #[test]
    fn test_equality() {
        let token1 = CasToken::new(Location::new(100), 5);
        let token2 = CasToken::new(Location::new(100), 5);
        let token3 = CasToken::new(Location::new(100), 6);
        let token4 = CasToken::new(Location::new(101), 5);

        assert_eq!(token1, token2);
        assert_ne!(token1, token3); // Different generation
        assert_ne!(token1, token4); // Different location
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Location and generation survive the token roundtrip for every
    /// 44-bit location and every generation.
    #[kani::proof]
    fn cas_token_roundtrip() {
        let raw: u64 = kani::any();
        kani::assume(raw <= Location::MAX_RAW);
        let generation: u16 = kani::any();
        let token = CasToken::new(Location::new(raw), generation);
        assert_eq!(token.location().as_raw(), raw);
        assert_eq!(token.generation(), generation);
    }

    /// Distinct seqlock versions never collide on a token for a fixed
    /// raw token value — the doc comment's "odd constant, bijective over
    /// u64" claim, certified. This is what makes CAS tokens observe
    /// every in-place increment.
    ///
    /// Deliberately scoped to a FIXED raw: cross-raw collisions
    /// (`mix(raw1, v1) == mix(raw2, v2)`) exist by pigeonhole for any
    /// 64-bit token and are not a goal — memcached's cas unique shares
    /// the property. The protocol's freshness argument only needs
    /// in-place updates at one (location, generation) to change the
    /// token, which is exactly what this establishes.
    ///
    /// # What is machine-checked, and what carries the rest
    ///
    /// The machine-checked certificate is `K * K_INV == 1 (mod 2^64)` —
    /// a constant equation, solver-trivial. Injectivity follows by ring
    /// arithmetic: `v1*K == v2*K` implies (multiplying both sides by
    /// K_INV, associativity/commutativity of wrapping multiplication)
    /// `v1 == v2`, and XOR with a fixed `raw` preserves injectivity.
    /// A full-width symbolic proof of the same theorem — either as the
    /// direct disequality or as the `(v*K)*K_INV == v` roundtrip — is
    /// deliberately NOT used: both forms build 64-bit multiplier
    /// circuits whose SAT cost proved erratic (seconds on one machine,
    /// 44+ minutes of CNF reduction on the CI runner before timeout).
    /// A 16-bit-bounded roundtrip keeps a symbolic sanity layer that a
    /// wrong K_INV or a broken `mix_version` expression still fails.
    #[kani::proof]
    fn mix_version_injective_in_version() {
        const K: u64 = 0x9E37_79B9_7F4A_7C15;
        /// Multiplicative inverse of K mod 2^64 (K is odd, so it exists).
        const K_INV: u64 = 0xF1DE_83E1_9937_733D;

        // The certificate: K really is a unit and K_INV really is its
        // inverse. Constant-folded — no symbolic multiplier.
        assert_eq!(K & 1, 1);
        assert_eq!(K.wrapping_mul(K_INV), 1);

        // Bounded symbolic sanity layer: recovering v from the token is
        // XOR-then-K_INV, over a domain small enough to keep the
        // multiplier cone tractable everywhere.
        let raw: u64 = kani::any();
        let v: u64 = kani::any();
        kani::assume(v < 1 << 16);
        assert_eq!((mix_version(raw, v) ^ raw).wrapping_mul(K_INV), v);
    }
}
