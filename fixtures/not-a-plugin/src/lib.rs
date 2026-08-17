//! A loadable dynamic library that is not a Cordis plugin (missing-symbol
//! test).
//!
//! The library opens successfully but exports no Cordis ABI symbols, so
//! `SoPlugin::load` rejects it with `LoadError::MissingSymbol` — the Rust
//! counterpart of the JS "invalid plugin" shape check at the dynamic
//! boundary.

/// An unrelated export proving the library is loadable.
#[unsafe(no_mangle)]
pub extern "C" fn not_a_cordis_plugin() -> u32 {
    42
}
