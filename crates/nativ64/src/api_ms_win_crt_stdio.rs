//! `api-ms-win-crt-stdio`: the default file translation mode (`_fmode`) and
//! the commit mode (`_commode`).
//!
//! `_fmode` is a process-wide `int` (0 until set: text mode). `__p__fmode` returns its address
//! so a caller can read and write it directly.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::invalid_parameter_err;
use std::sync::atomic::{AtomicI32, Ordering};

const EINVAL: i32 = 22;
const O_TEXT: i32 = 0x4000;
const O_BINARY: i32 = 0x8000;

static FMODE: AtomicI32 = AtomicI32::new(0);
/// `_commode`: the default commit mode of streams (0 = no commit-to-disk on flush).
static COMMODE: AtomicI32 = AtomicI32::new(0);

win32! {
    module "api-ms-win-crt-stdio" => "ucrtbase";
    Native fn __p__commode() -> *mut i32 { COMMODE.as_ptr() }
    Native fn __p__fmode() -> *mut i32 { FMODE.as_ptr() }
    // Only `_O_TEXT` and `_O_BINARY` are accepted (`_O_WTEXT` and friends are not modelled).
    Native fn _set_fmode(mode: i32) -> i32 {
        if mode != O_TEXT && mode != O_BINARY {
            return invalid_parameter_err(EINVAL);
        }
        FMODE.store(mode, Ordering::Relaxed);
        0
    }
    Native fn _get_fmode(mode: *mut i32) -> i32 {
        if mode.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        *mode = FMODE.load(Ordering::Relaxed);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{errno, test_support::*};

    #[test]
    fn registered_both_names() {
        for n in ["__p__commode", "__p__fmode", "_set_fmode", "_get_fmode"] {
            assert!(crate::lookup("api-ms-win-crt-stdio-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("ucrtbase", n).is_some());
        }
    }

    #[test]
    fn fmode_round_trip() {
        let _g = install_counting_handler();
        unsafe {
            let mut old = -1;
            assert_eq!(_get_fmode(&mut old), 0);
            assert_eq!(_set_fmode(O_BINARY), 0);
            let mut m = -1;
            assert_eq!(_get_fmode(&mut m), 0);
            assert_eq!(m, O_BINARY);
            assert_eq!(*__p__fmode(), O_BINARY);
            *__p__fmode() = O_TEXT;
            assert_eq!(_get_fmode(&mut m), 0);
            assert_eq!(m, O_TEXT);
            FMODE.store(old, Ordering::Relaxed);
        }
    }

    #[test]
    fn commode_is_writable_through_pointer() {
        unsafe {
            let p = __p__commode();
            assert_eq!(*p, 0);
            *p = 0x4000;
            assert_eq!(COMMODE.load(Ordering::Relaxed), 0x4000);
            *p = 0;
        }
    }

    #[test]
    fn fmode_errors() {
        let _g = install_counting_handler();
        unsafe {
            assert_eq!(_set_fmode(0x1234), EINVAL);
            assert_eq!(errno(), EINVAL);
            assert_eq!(_get_fmode(std::ptr::null_mut()), EINVAL);
            assert_eq!(calls(), 2);
        }
    }
}
