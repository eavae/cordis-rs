//! Facade crate for Cordis (Rust port).
//!
//! Re-exports the public API of `cordis-core` and `cordis-loader`.

pub use cordis_core::*;
pub use cordis_loader::*;

#[cfg(test)]
mod tests {
    /// Guards against a glob re-export conflict silently dropping an item.
    #[test]
    fn facade_reexports_core_and_loader_types() {
        fn _assert(
            _: Option<crate::Context>,
            _: Option<crate::Loader>,
            _: Option<crate::EntryOptions>,
            _: Option<crate::Effect>,
        ) {
        }
        let _ = _assert;
    }
}
