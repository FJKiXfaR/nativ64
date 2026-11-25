//! `chad.dll`: the console's DSP module (`XDsp*`, see `docs/audio.md`).
//!
//! A title connects to the processor's DSP, activates it and submits commands to it. Nothing is
//! run here: connecting returns a handle, and activating, submitting, deactivating and
//! disconnecting accept it and do nothing. A handle that is null or was not returned by a
//! connect (or was already disconnected) is refused.

use crate::com::{E_INVALIDARG, HRESULT, S_OK};
use std::ffi::c_void;
use std::sync::Mutex;

/// What a handle points at (the stream limit is kept, nothing reads it yet).
#[allow(dead_code)]
struct Dsp {
    streams: u32,
}

/// The live connections, as addresses: a handle is valid only while it is in here.
static LIVE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn live() -> std::sync::MutexGuard<'static, Vec<usize>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn valid(handle: *mut c_void) -> bool {
    !handle.is_null() && live().contains(&(handle as usize))
}

/// The status of a command on `handle`: accepted when it is a live connection.
fn accept(handle: *mut c_void) -> HRESULT {
    if valid(handle) {
        S_OK
    } else {
        E_INVALIDARG
    }
}

win32! {
    module "chad";
    // (a, b, c, maximum streams, flags, &handle)
    Native fn XDspConnectWithMaximumStreamLimit(_a: usize, _b: u32, _c: u32, streams: u32,
                                                _flags: u32, handle: *mut *mut c_void) -> HRESULT {
        if handle.is_null() {
            return E_INVALIDARG;
        }
        let p = Box::into_raw(Box::new(Dsp { streams })).cast::<c_void>();
        live().push(p as usize);
        handle.write(p);
        S_OK
    }
    Native fn XDspActivate(handle: *mut c_void) -> HRESULT {
        accept(handle)
    }
    Native fn XDspSubmitCommand(handle: *mut c_void) -> HRESULT {
        accept(handle)
    }
    Native fn XDspDeactivate(handle: *mut c_void) -> HRESULT {
        accept(handle)
    }
    // Frees the connection; the handle is refused afterwards.
    Native fn XDspDisconnect(handle: *mut c_void) -> HRESULT {
        let mut l = live();
        let Some(i) = l.iter().position(|&p| p == handle as usize) else {
            return E_INVALIDARG;
        };
        l.swap_remove(i);
        drop(l);
        drop(Box::from_raw(handle.cast::<Dsp>()));
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;
    use std::ptr::null_mut;

    unsafe fn connect(streams: u32) -> *mut c_void {
        let mut h = null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(0, 0, 0, streams, 1, &mut h),
            S_OK
        );
        assert!(!h.is_null());
        h
    }

    #[test]
    fn exports_resolve_by_file_name() {
        for n in [
            "XDspActivate",
            "XDspConnectWithMaximumStreamLimit",
            "XDspDeactivate",
            "XDspDisconnect",
            "XDspSubmitCommand",
        ] {
            assert!(lookup("chad.dll", n).is_some(), "{n}");
        }
    }

    #[test]
    fn connect_activate_submit_deactivate_disconnect() {
        unsafe {
            let h = connect(0x80);
            assert_eq!((*h.cast::<Dsp>()).streams, 0x80);
            assert_eq!(XDspActivate(h), S_OK);
            assert_eq!(XDspSubmitCommand(h), S_OK);
            assert_eq!(XDspSubmitCommand(h), S_OK);
            assert_eq!(XDspDeactivate(h), S_OK);
            assert_eq!(XDspDisconnect(h), S_OK);
        }
    }

    #[test]
    fn connect_needs_a_place_for_the_handle() {
        unsafe {
            assert_eq!(
                XDspConnectWithMaximumStreamLimit(0, 0, 0, 1, 0, null_mut()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn bad_handles_are_refused() {
        unsafe {
            let mut local = 0u64;
            let bogus = (&mut local as *mut u64).cast::<c_void>();
            for h in [null_mut(), bogus] {
                assert_eq!(XDspActivate(h), E_INVALIDARG);
                assert_eq!(XDspSubmitCommand(h), E_INVALIDARG);
                assert_eq!(XDspDeactivate(h), E_INVALIDARG);
                assert_eq!(XDspDisconnect(h), E_INVALIDARG);
            }
        }
    }

    #[test]
    fn a_disconnected_handle_is_refused() {
        unsafe {
            let h = connect(4);
            assert_eq!(XDspDisconnect(h), S_OK);
            assert_eq!(XDspActivate(h), E_INVALIDARG);
            assert_eq!(XDspDisconnect(h), E_INVALIDARG);
        }
    }
}
