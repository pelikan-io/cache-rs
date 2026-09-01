//! Emits the `model_checking` cfg when either model-checking backend
//! (`loom` or `shuttle`) is enabled.
//!
//! Sites that must change for ANY model checker — wider model atomics
//! breaking `size_of == 64` asserts and SIMD raw-`u64` loads, shrunken
//! stripe counts, deterministic RNG, std-thread test modules — gate on
//! `#[cfg(model_checking)]` / `#[cfg(not(model_checking))]` instead of
//! naming each backend, so adding a backend cannot silently miss a site.
//! Backend-specific code (the `crate::sync` re-exports, the model test
//! modules themselves) still names its feature, with `loom` taking
//! precedence when both are enabled (e.g. under `--all-features`).

fn main() {
    println!("cargo::rustc-check-cfg=cfg(model_checking)");
    if std::env::var_os("CARGO_FEATURE_LOOM").is_some()
        || std::env::var_os("CARGO_FEATURE_SHUTTLE").is_some()
    {
        println!("cargo::rustc-cfg=model_checking");
    }
}
