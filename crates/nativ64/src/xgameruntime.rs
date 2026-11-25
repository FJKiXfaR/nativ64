//! `xgameruntime`: the console game runtime DLL a title loads at startup.
//!
//! The title's runtime thunk library does `LoadLibraryExW("xgameruntime.dll")`, resolves the
//! six bootstrap exports below with `GetProcAddress` and calls `InitializeApiImplEx2` with the
//! runtime version and flags. Individual runtime APIs (`XSystem*`, `XUser*`, ...) are then
//! fetched one by one through `QueryApiImpl`.
//!
//! The bootstrap calls are all that is modelled so far: initialization succeeds and keeps a
//! count, `UninitializeApiImpl` undoes one. `QueryApiImpl` reports every API as unavailable
//! (`E_NOTIMPL`) until the individual APIs are implemented. `XErrorReport` is accepted and
//! ignored. No Wine oracle exists for this module (Wine has no game runtime).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

pub const S_OK: i32 = 0;
pub const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;

static INIT_COUNT: AtomicU32 = AtomicU32::new(0);

/// Number of active `InitializeApiImpl*` calls (not yet balanced by `UninitializeApiImpl`).
pub fn init_count() -> u32 {
    INIT_COUNT.load(Ordering::SeqCst)
}

fn initialize() -> i32 {
    INIT_COUNT.fetch_add(1, Ordering::SeqCst);
    S_OK
}

win32! {
    module "xgameruntime";
    // (api id, api version, out function table): no API is available yet.
    Native fn QueryApiImpl(_a: u64, _b: u64, out: *mut *const c_void) -> i32 {
        if !out.is_null() {
            *out = std::ptr::null();
        }
        E_NOTIMPL
    }
    // Pre-versioned entry: (major, minor).
    Native fn InitializeApiImpl(_major: u32, _minor: u32) -> i32 {
        initialize()
    }
    Native fn InitializeApiImplEx(_version: u64, _api: u64, _flags: u32) -> i32 {
        initialize()
    }
    // `out` receives the runtime's internal context handle when not NULL.
    Native fn InitializeApiImplEx2(_version: u64, _api: u64, _flags: u32,
                                   out: *mut *const c_void) -> i32 {
        if !out.is_null() {
            *out = std::ptr::null();
        }
        initialize()
    }
    Native fn UninitializeApiImpl() -> i32 {
        let _ = INIT_COUNT.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        S_OK
    }
    // (hresult, message): diagnostics only.
    Native fn XErrorReport(_hr: i32, _msg: *const c_void) -> i32 {
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;

    #[test]
    fn bootstrap_exports_are_resolvable_by_file_name() {
        for n in [
            "QueryApiImpl",
            "InitializeApiImpl",
            "InitializeApiImplEx",
            "InitializeApiImplEx2",
            "UninitializeApiImpl",
            "XErrorReport",
        ] {
            assert!(lookup("xgameruntime.dll", n).is_some(), "{n}");
        }
    }

    #[test]
    fn init_and_uninit_balance() {
        unsafe {
            let base = init_count();
            let mut out = std::ptr::dangling::<c_void>();
            assert_eq!(InitializeApiImplEx2(1, 2, 6, &mut out), S_OK);
            assert!(out.is_null());
            assert_eq!(InitializeApiImplEx(1, 2, 6), S_OK);
            assert_eq!(init_count(), base + 2);
            assert_eq!(UninitializeApiImpl(), S_OK);
            assert_eq!(UninitializeApiImpl(), S_OK);
            assert_eq!(init_count(), base);
            assert_eq!(QueryApiImpl(1, 2, &mut out), E_NOTIMPL);
        }
    }
}
