//! `api-ms-win-crt-runtime`: the UCRT per-thread `errno` and the invalid parameter handler.
//!
//! `errno` is a per-thread `int` private to this crate (not glibc's `errno`). `_errno` returns
//! its address, so a caller can read and write it directly.
//!
//! The invalid parameter handler is looked up thread-local first, then global. The exported
//! `_invalid_parameter*` functions terminate the process when no handler is installed (the UCRT
//! raises a fast-fail); if the handler returns, `_invalid_parameter` returns and
//! `_invalid_parameter_noinfo_noreturn` terminates anyway.
//!
//! Deviation: the crate-internal [`invalid_parameter`], used by the `_s` functions of the other
//! modules, returns when no handler is installed instead of terminating, so that error paths
//! stay testable and a game that ignores the returned error code keeps running.
//!
//! `__setusermatherr` lives in `api_ms_win_crt_math` (its handler is read from there).
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

const EINVAL: i32 = 22;

/// `_invalid_parameter_handler`: `(expression, function, file, line, reserved)`.
pub type InvalidParameterHandler =
    unsafe extern "win64" fn(*const u16, *const u16, *const u16, u32, usize);

static GLOBAL_HANDLER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ERRNO: Cell<i32> = const { Cell::new(0) };
    static THREAD_HANDLER: Cell<usize> = const { Cell::new(0) };
}

/// Sets the calling thread's CRT `errno`.
pub(crate) fn set_errno(v: i32) {
    ERRNO.with(|e| e.set(v));
}

/// Reads the calling thread's CRT `errno`.
#[cfg(test)]
pub(crate) fn errno() -> i32 {
    ERRNO.with(|e| e.get())
}

/// The handler in effect (thread-local first), or 0.
fn current_handler() -> usize {
    let t = THREAD_HANDLER.with(|h| h.get());
    if t != 0 {
        t
    } else {
        GLOBAL_HANDLER.load(Ordering::Relaxed)
    }
}

/// Calls the handler if there is one; returns whether it was called.
unsafe fn call_handler(
    expr: *const u16,
    func: *const u16,
    file: *const u16,
    line: u32,
    res: usize,
) -> bool {
    let h = current_handler();
    if h == 0 {
        return false;
    }
    let f: InvalidParameterHandler = std::mem::transmute(h);
    f(expr, func, file, line, res);
    true
}

/// Invokes the invalid parameter handler without information (for the `_s` functions of other
/// modules). Sets nothing; returns when the handler returns or when none is installed.
pub(crate) fn invalid_parameter() {
    unsafe {
        call_handler(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, 0);
    }
}

/// Sets `errno` to `err`, invokes the handler, and returns `err` (the `_VALIDATE_RETURN_ERRCODE`
/// pattern).
pub(crate) fn invalid_parameter_err(err: i32) -> i32 {
    set_errno(err);
    invalid_parameter();
    err
}

fn terminate() -> ! {
    eprintln!("nativ64: invalid parameter passed to a C runtime function (no handler installed): terminating");
    std::process::abort()
}

win32! {
    module "api-ms-win-crt-runtime" => "ucrtbase";
    Native fn _errno() -> *mut i32 { ERRNO.with(|e| e.as_ptr()) }
    Native fn _get_errno(out: *mut i32) -> i32 {
        if out.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        *out = ERRNO.with(|e| e.get());
        0
    }
    Native fn _set_errno(v: i32) -> i32 {
        set_errno(v);
        0
    }
    Native fn _set_invalid_parameter_handler(h: usize) -> usize {
        GLOBAL_HANDLER.swap(h, Ordering::Relaxed)
    }
    Native fn _get_invalid_parameter_handler() -> usize {
        GLOBAL_HANDLER.load(Ordering::Relaxed)
    }
    Native fn _set_thread_local_invalid_parameter_handler(h: usize) -> usize {
        THREAD_HANDLER.with(|c| c.replace(h))
    }
    Native fn _get_thread_local_invalid_parameter_handler() -> usize {
        THREAD_HANDLER.with(|c| c.get())
    }
    Native fn _invalid_parameter(
        expr: *const u16, func: *const u16, file: *const u16, line: u32, res: usize
    ) {
        if !call_handler(expr, func, file, line, res) {
            terminate();
        }
    }
    Native fn _invalid_parameter_noinfo() {
        if !call_handler(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, 0) {
            terminate();
        }
    }
    Native fn _invalid_parameter_noinfo_noreturn() {
        call_handler(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, 0);
        terminate();
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Serializes tests that install a global handler and counts its calls (per thread).
    use std::cell::Cell;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());
    thread_local! { static CALLS: Cell<u32> = const { Cell::new(0) }; }

    unsafe extern "win64" fn counting(
        _: *const u16,
        _: *const u16,
        _: *const u16,
        _: u32,
        _: usize,
    ) {
        CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Holds the global handler (a returning, counting one) for the guard's lifetime.
    pub struct HandlerGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    pub fn install_counting_handler() -> HandlerGuard {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        CALLS.with(|c| c.set(0));
        unsafe { super::_set_invalid_parameter_handler(counting as *const () as usize) };
        HandlerGuard(g)
    }

    pub fn calls() -> u32 {
        CALLS.with(|c| c.get())
    }

    impl Drop for HandlerGuard {
        fn drop(&mut self) {
            unsafe { super::_set_invalid_parameter_handler(0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn get(name: &str) -> *const () {
        crate::lookup("api-ms-win-crt-runtime-l1-1-0.dll", name)
            .unwrap()
            .addr
    }

    #[test]
    fn every_name_registered() {
        let names = "_errno _get_errno _set_errno _set_invalid_parameter_handler _get_invalid_parameter_handler _set_thread_local_invalid_parameter_handler _get_thread_local_invalid_parameter_handler _invalid_parameter _invalid_parameter_noinfo _invalid_parameter_noinfo_noreturn";
        assert_eq!(names.split(' ').count(), 10);
        for n in names.split(' ') {
            assert!(crate::lookup("api-ms-win-crt-runtime", n).is_some(), "{n}");
            assert!(crate::lookup("ucrtbase", n).is_some(), "{n}");
        }
        let f: unsafe extern "win64" fn() -> *mut i32 =
            unsafe { std::mem::transmute(get("_errno")) };
        assert_eq!(unsafe { f() }, unsafe { _errno() });
    }

    #[test]
    fn errno_roundtrip_and_pointer() {
        unsafe {
            assert_eq!(_set_errno(34), 0);
            assert_eq!(*_errno(), 34);
            *_errno() = 7;
            let mut v = 0;
            assert_eq!(_get_errno(&mut v), 0);
            assert_eq!(v, 7);
        }
    }

    #[test]
    fn errno_is_per_thread() {
        unsafe { _set_errno(11) };
        std::thread::spawn(|| unsafe {
            assert_eq!(*_errno(), 0);
            _set_errno(22);
            assert_eq!(errno(), 22);
        })
        .join()
        .unwrap();
        assert_eq!(errno(), 11);
        // Distinct from glibc's errno.
        unsafe { *libc::__errno_location() = 99 };
        assert_eq!(errno(), 11);
    }

    #[test]
    fn get_errno_null_is_invalid_parameter() {
        let _g = install_counting_handler();
        unsafe {
            _set_errno(0);
            assert_eq!(_get_errno(std::ptr::null_mut()), EINVAL);
        }
        assert_eq!(errno(), EINVAL);
        assert_eq!(calls(), 1);
    }

    #[test]
    fn handler_precedence() {
        unsafe extern "win64" fn thread_h(
            _: *const u16,
            _: *const u16,
            _: *const u16,
            _: u32,
            _: usize,
        ) {
            THREAD_CALLS.with(|c| c.set(c.get() + 1));
        }
        thread_local! { static THREAD_CALLS: Cell<u32> = const { Cell::new(0) }; }
        let _g = install_counting_handler();
        unsafe {
            assert_eq!(_get_thread_local_invalid_parameter_handler(), 0);
            assert_eq!(
                _set_thread_local_invalid_parameter_handler(thread_h as *const () as usize),
                0
            );
            assert_eq!(
                _get_thread_local_invalid_parameter_handler(),
                thread_h as *const () as usize
            );
            _invalid_parameter_noinfo();
            assert_eq!(THREAD_CALLS.with(|c| c.get()), 1);
            assert_eq!(calls(), 0);
            // Another thread sees only the global handler.
            let other = std::thread::spawn(|| {
                _invalid_parameter(std::ptr::null(), std::ptr::null(), std::ptr::null(), 5, 0);
                calls()
            })
            .join()
            .unwrap();
            assert_eq!(other, 1);
            assert_eq!(calls(), 0);
            assert_eq!(
                _set_thread_local_invalid_parameter_handler(0),
                thread_h as *const () as usize
            );
            _invalid_parameter_noinfo();
            assert_eq!(calls(), 1);
            assert_ne!(_get_invalid_parameter_handler(), 0);
        }
    }

    #[test]
    fn handler_set_returns_previous() {
        let _g = install_counting_handler();
        unsafe {
            let cur = _get_invalid_parameter_handler();
            assert_ne!(cur, 0);
            assert_eq!(_set_invalid_parameter_handler(0), cur);
            assert_eq!(_get_invalid_parameter_handler(), 0);
            assert_eq!(_set_invalid_parameter_handler(cur), 0);
        }
    }

    #[test]
    fn internal_helper_without_handler_returns() {
        let _g = install_counting_handler();
        unsafe { _set_invalid_parameter_handler(0) };
        invalid_parameter();
        assert_eq!(calls(), 0);
    }
}
