//! Fuzz target: arbitrary bytes must never panic and must never read out of
//! bounds (the latter is what the sanitizer catches). This exercises the
//! whole decode surface, not just the header: a valid header followed by
//! garbage entries must fail cleanly through `Block::parse`'s full
//! validation walk, not panic partway through.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(hdr) = ziplist::BlockHeader::parse(data) {
        let _ = ziplist::Block::parse(data); // full validation walk
        let _ = hdr;
    }
});
