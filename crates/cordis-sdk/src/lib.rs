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

pub mod abi;
pub use abi::{
    BoxedFuture, ContextBridge, HostVtable, PLUGIN_API_VERSION, PluginDisposer,
    PluginEventCallback, PluginHandle, RcWaker, Spawned, WAKER_VTABLE, WakerData, spawn,
    waker_from_raw,
};
#[cfg(feature = "abi-exports")]
pub use abi::{plugin_api_version, plugin_create, plugin_dispose};

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn noop_log(_message: *const std::ffi::c_char) {}

    unsafe extern "C" fn noop_spawn(_data: *mut std::ffi::c_void, _future: *mut std::ffi::c_void) {}

    unsafe extern "C" fn noop_provide(
        _handle: *mut PluginHandle,
        _name: *const std::ffi::c_char,
        _payload: *const std::ffi::c_char,
    ) -> i32 {
        0
    }

    unsafe extern "C" fn noop_get(
        _handle: *mut PluginHandle,
        _name: *const std::ffi::c_char,
    ) -> *const std::ffi::c_char {
        std::ptr::null()
    }

    unsafe extern "C" fn noop_on(
        _handle: *mut PluginHandle,
        _event: *const std::ffi::c_char,
        _callback: super::PluginEventCallback,
    ) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    unsafe extern "C" fn noop_emit(
        _handle: *mut PluginHandle,
        _event: *const std::ffi::c_char,
        _payload: *const std::ffi::c_char,
    ) {
    }

    unsafe extern "C" fn noop_effect_disposer(
        _handle: *mut PluginHandle,
        _disposer: super::PluginDisposer,
    ) {
    }

    #[test]
    fn abi_symbols_are_exported() {
        assert_eq!(plugin_api_version(), PLUGIN_API_VERSION);
        // SAFETY: the vtable is valid for the call.
        let vtable = abi::HostVtable {
            log: noop_log,
            spawn: noop_spawn,
            provide: noop_provide,
            get: noop_get,
            on: noop_on,
            emit: noop_emit,
            effect_disposer: noop_effect_disposer,
            data: std::ptr::null_mut(),
            host_version: PLUGIN_API_VERSION,
        };
        let handle = unsafe { plugin_create(&vtable) };
        assert!(!handle.is_null());
        // SAFETY: handle comes from plugin_create.
        unsafe { plugin_dispose(handle) };
    }

    #[test]
    fn contract_types_are_re_exported() {
        fn _assert_contract(_: Option<Context>, _: Option<EffectHandle>, _: Option<Fiber>) {}
        let _ = _assert_contract;
    }
}
