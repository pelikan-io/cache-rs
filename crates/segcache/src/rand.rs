//! Random number generator initialization.

use ::rand::SeedableRng;

pub type Random = rand_xoshiro::Xoshiro256PlusPlus;

/// Creates a freshly-seeded [`Random`]. Test builds seed from a fixed
/// value for reproducibility; other builds seed from system entropy.
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
