// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Random number generator initialization.
//!
//! A fast, non-cryptographic PRNG is sufficient here: it only drives
//! eviction sampling, never anything security-sensitive.

use ::rand::SeedableRng;

/// The PRNG used for eviction sampling.
pub type Random = rand_xoshiro::Xoshiro256PlusPlus;

/// Creates a freshly-seeded [`Random`].
///
/// In `test` builds it is seeded from a fixed value so runs are
/// reproducible; otherwise it is seeded from the system entropy source.
pub fn rng() -> Random {
    #[cfg(test)]
    {
        Random::seed_from_u64(0)
    }
    #[cfg(not(test))]
    {
        Random::from_rng(&mut ::rand::rng())
    }
}
