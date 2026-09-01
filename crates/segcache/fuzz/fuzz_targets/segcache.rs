//! Differential fuzzing of the segcache public API against a model.
//!
//! The old target only checked crash-freedom (and had been dead since the
//! concurrency rewrite: it built with `hash_power(5)`, below the
//! hashtable's `power >= 7` assert, so every input panicked in the
//! builder). This one mirrors each operation into a `HashMap` model and
//! asserts the directional contract eviction allows:
//!
//! - the MODEL IS A SUPERSET of the cache: eviction may drop any item
//!   from the cache, so a cache miss is always legal;
//! - a cache HIT must agree with the model: same value, and the model
//!   must still hold the key — a hit for a key the model deleted (or
//!   never inserted) is a resurrection/corruption bug, and a hit with a
//!   different value is a lost-update/aliasing bug. This is exactly the
//!   class the concurrency work kept finding ("get returned another
//!   key's value") expressed as a single-threaded oracle;
//! - numeric ops that succeed must match the model's arithmetic exactly.
//!
//! TTLs are deliberately either ZERO (never expires) or >= 1 hour, so no
//! item can lazily expire mid-run and blur the hit assertions (the
//! coarse clock ticks in real seconds while libFuzzer runs for minutes).
//! Expiry behavior itself is covered by the unit/integration suites.
//!
//! Every input ends with `check_integrity()` and an item-count bound
//! (both under the `debug` feature, which this fuzz crate enables).

#![no_main]
use libfuzzer_sys::fuzz_target;

use std::collections::HashMap;
use std::time::Duration;

use segcache::{Policy, Segcache, Value};

const SEG_SIZE: i32 = 4096;
// Four segments: with libFuzzer inputs capped at -max_len=65536 (set in
// CI and the local runs), a dense input can store ~170KB into this 16KB
// heap, so eviction and the NoFreeSegments/retry paths are genuinely
// exercised — at 16 segments under the default 4096-byte max_len they
// were dead code (an input could store ~13KB into a 64KB heap and never
// evict once). Four rather than fewer so Merge's held-back spare and
// S3-FIFO's admission/main split aren't degenerate.
const HEAP_SIZE: usize = 4 * 4096;
const HASH_POWER: u8 = 7; // the minimum the hashtable accepts

#[derive(Clone, PartialEq, Debug)]
enum MVal {
    Bytes(Vec<u8>),
    Num(u64),
}

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u64(&mut self) -> Option<u64> {
        let mut v = [0u8; 8];
        for b in v.iter_mut() {
            *b = self.u8()?;
        }
        Some(u64::from_be_bytes(v))
    }
    /// A 1..=255 byte key (klen is a u8 and empty keys are rejected).
    fn key(&mut self) -> Option<&'a [u8]> {
        let len = (self.u8()? as usize).max(1);
        let end = self.pos.checked_add(len)?;
        let k = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(k)
    }
    /// A value up to ~2 pages, so items always fit a segment.
    fn value(&mut self) -> Option<&'a [u8]> {
        let len = usize::from(self.u8()?) * 8;
        let end = self.pos.checked_add(len)?;
        let v = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(v)
    }
    /// ZERO or >= 1h — never expires within a fuzz run (see module doc).
    fn ttl(&mut self) -> Option<Duration> {
        let b = self.u8()?;
        Some(if b == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(3600 + b as u64)
        })
    }
}

fn check_hit(model: &HashMap<Vec<u8>, MVal>, key: &[u8], value: Value) {
    match model.get(key) {
        None => panic!(
            "RESURRECTION: cache hit for a key the model deleted or never inserted (key={key:?})"
        ),
        Some(MVal::Bytes(expected)) => match value {
            Value::Bytes(got) => assert_eq!(
                got,
                &expected[..],
                "VALUE MISMATCH on bytes item (key={key:?})"
            ),
            Value::U64(got) => {
                panic!("TYPE MISMATCH: model has bytes, cache returned numeric {got} (key={key:?})")
            }
        },
        Some(MVal::Num(expected)) => match value {
            Value::U64(got) => {
                assert_eq!(got, *expected, "VALUE MISMATCH on numeric item (key={key:?})")
            }
            Value::Bytes(_) => {
                panic!("TYPE MISMATCH: model has numeric, cache returned bytes (key={key:?})")
            }
        },
    }
}

fuzz_target!(|data: &[u8]| {
    // The first byte selects the eviction policy, so the relocation
    // machinery (merge's copy_into, S3-FIFO's promote — the code the
    // concurrency hardening kept finding bugs in) runs under the oracle,
    // not just Random's whole-segment drops.
    let mut input = Input { data, pos: 0 };
    let policy = match input.u8() {
        None => return,
        Some(b) => match b % 3 {
            0 => Policy::Random,
            1 => Policy::Merge {
                max: 8,
                merge: 4,
                compact: 2,
            },
            _ => Policy::S3Fifo {
                admission_ratio: 0.25,
            },
        },
    };
    let cache = Segcache::builder()
        .segment_size(SEG_SIZE)
        .heap_size(HEAP_SIZE)
        .hash_power(HASH_POWER)
        .eviction(policy)
        .build()
        .expect("failed to create cache");
    let mut model: HashMap<Vec<u8>, MVal> = HashMap::new();

    while let Some(op) = input.u8() {
        match op % 10 {
            // insert bytes
            0 => {
                let (Some(key), Some(value), Some(ttl)) =
                    (input.key(), input.value(), input.ttl())
                else {
                    break;
                };
                if cache.insert(key, value, None, ttl).is_ok() {
                    model.insert(key.to_vec(), MVal::Bytes(value.to_vec()));
                }
                // on failure the previous entry (if any) must survive —
                // covered by the next get's hit assertion.
            }
            // insert numeric
            1 => {
                let (Some(key), Some(v), Some(ttl)) = (input.key(), input.u64(), input.ttl())
                else {
                    break;
                };
                if cache.insert(key, v, None, ttl).is_ok() {
                    model.insert(key.to_vec(), MVal::Num(v));
                }
            }
            // get
            2 => {
                let Some(key) = input.key() else { break };
                if let Some(item) = cache.get(key) {
                    check_hit(&model, key, item.value());
                }
                // miss: always legal (eviction), even when the model has it.
            }
            // delete
            3 => {
                let Some(key) = input.key() else { break };
                cache.delete(key);
                // remove from the model regardless of the ack: an un-acked
                // delete means the cache didn't have it (evicted), and the
                // model must not expect it back either way.
                model.remove(key);
            }
            // incr / decr
            4 | 5 => {
                let (Some(key), Some(rhs)) = (input.key(), input.u64()) else {
                    break;
                };
                let result = if op % 10 == 4 {
                    cache.wrapping_add(key, rhs)
                } else {
                    cache.saturating_sub(key, rhs)
                };
                if let Ok(new) = result {
                    // Success implies the cache had a numeric item; the
                    // model (a superset) must agree on type and value.
                    match model.get_mut(key) {
                        Some(MVal::Num(m)) => {
                            *m = if op % 10 == 4 {
                                m.wrapping_add(rhs)
                            } else {
                                m.saturating_sub(rhs)
                            };
                            assert_eq!(new, *m, "NUMERIC DIVERGENCE (key={key:?})");
                        }
                        Some(MVal::Bytes(_)) => {
                            panic!("TYPE MISMATCH: numeric op succeeded on a bytes key")
                        }
                        None => panic!("RESURRECTION: numeric op succeeded on a deleted key"),
                    }
                }
                // NotFound / NotNumeric: legal (evicted, or bytes item).
            }
            // cas with the CURRENT token (gets-then-cas): must behave as
            // a replace when the item is resident.
            6 => {
                let (Some(key), Some(value), Some(ttl)) =
                    (input.key(), input.value(), input.ttl())
                else {
                    break;
                };
                let token = match cache.get(key) {
                    Some(item) => {
                        check_hit(&model, key, item.value());
                        item.cas()
                    }
                    None => continue,
                };
                if cache.cas(key, value, None, ttl, token).is_ok() {
                    model.insert(key.to_vec(), MVal::Bytes(value.to_vec()));
                }
                // Err: the item moved/expired/evicted between the gets
                // and the cas — single-threaded here, but eviction inside
                // cas's own reserve path can still legally interleave.
            }
            // cas with an arbitrary (almost certainly stale) token:
            // exercises the failure path; a success is a legal token
            // collision, in which case the value really was written.
            7 => {
                let (Some(key), Some(value), Some(ttl), Some(token)) =
                    (input.key(), input.value(), input.ttl(), input.u64())
                else {
                    break;
                };
                if cache.cas(key, value, None, ttl, token).is_ok() {
                    model.insert(key.to_vec(), MVal::Bytes(value.to_vec()));
                }
            }
            // expire: no item can be past-deadline (see ttl()), so this
            // must reclaim nothing that a later hit would miss — covered
            // by the superset direction of the hit assertions.
            8 => {
                cache.expire();
            }
            // clear
            9 => {
                cache.clear();
                model.clear();
            }
            _ => unreachable!(),
        }
    }

    // End-of-input sweep: internal invariants and the superset bound.
    cache.check_integrity().expect("integrity check failed");
    assert!(
        cache.items() <= model.len(),
        "cache holds {} items but the model (a superset) holds {}",
        cache.items(),
        model.len()
    );
});
