//! Cordis plugin SDK.
//!
//! The only crate a `.so` plugin needs to depend on. Re-exports the plugin
//! contract from `cordis-core` and declares the ABI entry points (story card
//! E1; the protocol is finalized in E2).

pub use cordis_core::{
    ApplyFn, Context, CordisError, Effect, EffectHandle, Fiber, FiberError, FiberState, Plugin,
    Service, async_disposer, disposer, sync_disposer,
};
pub use cordis_macros::{inject, service};

/// The ABI version implemented by this SDK.
pub const PLUGIN_API_VERSION: u32 = 1;

/// Returns the plugin ABI version the host must match.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    PLUGIN_API_VERSION
}

/// Placeholder plugin entry (finalized in E2).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_create() -> *mut () {
    std::ptr::null_mut()
}

/// Placeholder plugin teardown (finalized in E2).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_dispose(_handle: *mut ()) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_symbols_are_exported() {
        assert_eq!(plugin_api_version(), PLUGIN_API_VERSION);
        assert!(plugin_create().is_null());
    }

    #[test]
    fn contract_types_are_re_exported() {
        fn _assert_contract(_: Option<Context>, _: Option<EffectHandle>, _: Option<Fiber>) {}
        let _ = _assert_contract;
    }
}
