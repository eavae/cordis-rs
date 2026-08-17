//! Facade crate for Cordis (Rust port).
//!
//! Re-exports the public API of `cordis-core`, mirroring the npm `cordis`
//! package (`packages/core`). Loader types live in the separate
//! `cordis-loader` crate, matching `@cordisjs/plugin-loader`.

pub use cordis_core::*;

#[cfg(test)]
mod tests {
    /// Guards against a glob re-export conflict silently dropping an item.
    #[test]
    fn facade_reexports_core_types() {
        fn _assert(_: Option<crate::Context>, _: Option<crate::Fiber>, _: Option<crate::Effect>) {}
        let _ = _assert;
    }
}
