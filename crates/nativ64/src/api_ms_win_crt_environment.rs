//! `api-ms-win-crt-environment`: `_putenv_s` and `_wgetcwd`, on the process environment and
//! working directory.
//!
//! Deviation: `_wgetcwd` returns the Linux path as is (no drive letter).

use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use std::ffi::{c_char, CStr};

const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

/// Sets (or, for an empty value, removes) an environment variable; returns 0 or an errno value.
fn put_env(name: &[u8], value: &[u8]) -> i32 {
    if name.is_empty() || name.contains(&b'=') {
        return invalid_parameter_err(EINVAL);
    }
    let (Ok(n), Ok(v)) = (std::ffi::CString::new(name), std::ffi::CString::new(value)) else {
        return invalid_parameter_err(EINVAL);
    };
    let r = unsafe {
        if value.is_empty() {
            libc::unsetenv(n.as_ptr())
        } else {
            libc::setenv(n.as_ptr(), v.as_ptr(), 1)
        }
    };
    if r != 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    0
}

win32! {
    module "api-ms-win-crt-environment" => "ucrtbase";
    Native fn _putenv_s(name: *const c_char, value: *const c_char) -> i32 {
        if name.is_null() || value.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        put_env(CStr::from_ptr(name).to_bytes(), CStr::from_ptr(value).to_bytes())
    }
    Native fn _wgetcwd(buf: *mut u16, size: i32) -> *mut u16 {
        if size < 0 || (!buf.is_null() && size == 0) {
            invalid_parameter_err(EINVAL);
            return std::ptr::null_mut();
        }
        let Ok(dir) = std::env::current_dir() else {
            set_errno(2); // ENOENT
            return std::ptr::null_mut();
        };
        let wide: Vec<u16> = dir.to_string_lossy().encode_utf16().chain([0]).collect();
        if (size as usize) != 0 && wide.len() > size as usize {
            set_errno(ERANGE);
            return std::ptr::null_mut();
        }
        let out = if buf.is_null() {
            let p = libc::malloc(wide.len().max(size as usize) * 2) as *mut u16;
            if p.is_null() {
                set_errno(12); // ENOMEM
                return p;
            }
            p
        } else {
            buf
        };
        std::ptr::copy_nonoverlapping(wide.as_ptr(), out, wide.len());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{errno, test_support::*};

    #[test]
    fn registered_both_names() {
        for n in ["_putenv_s", "_wgetcwd"] {
            assert!(crate::lookup("api-ms-win-crt-environment-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("ucrtbase", n).is_some());
        }
    }

    #[test]
    fn putenv_sets_and_removes() {
        unsafe {
            assert_eq!(_putenv_s(c"N64_PUTENV_T1".as_ptr(), c"abc".as_ptr()), 0);
            assert_eq!(std::env::var("N64_PUTENV_T1").unwrap(), "abc");
            assert_eq!(_putenv_s(c"N64_PUTENV_T1".as_ptr(), c"".as_ptr()), 0);
            assert!(std::env::var("N64_PUTENV_T1").is_err());
            // Removing an unset variable is fine.
            assert_eq!(_putenv_s(c"N64_PUTENV_T1".as_ptr(), c"".as_ptr()), 0);
        }
    }

    #[test]
    fn putenv_errors() {
        let _g = install_counting_handler();
        unsafe {
            for (n, v) in [
                (std::ptr::null(), c"x".as_ptr()),
                (c"X".as_ptr(), std::ptr::null()),
                (c"".as_ptr(), c"x".as_ptr()),
                (c"A=B".as_ptr(), c"x".as_ptr()),
            ] {
                crate::api_ms_win_crt_runtime::_set_errno(0);
                assert_eq!(_putenv_s(n, v), EINVAL);
                assert_eq!(errno(), EINVAL);
            }
        }
        assert_eq!(calls(), 4);
        assert!(std::env::var("A").is_err());
    }

    #[test]
    fn wgetcwd_fills_and_allocates() {
        let want: Vec<u16> = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .encode_utf16()
            .collect();
        unsafe {
            let mut buf = vec![0u16; 4096];
            let r = _wgetcwd(buf.as_mut_ptr(), buf.len() as i32);
            assert_eq!(r, buf.as_mut_ptr());
            assert_eq!(&buf[..want.len()], &want[..]);
            assert_eq!(buf[want.len()], 0);

            let p = _wgetcwd(std::ptr::null_mut(), 0);
            assert!(!p.is_null());
            assert_eq!(std::slice::from_raw_parts(p, want.len()), &want[..]);
            assert_eq!(*p.add(want.len()), 0);
            libc::free(p as *mut _);
        }
    }

    #[test]
    fn wgetcwd_errors() {
        let _g = install_counting_handler();
        unsafe {
            let mut small = [0u16; 1];
            crate::api_ms_win_crt_runtime::_set_errno(0);
            assert!(_wgetcwd(small.as_mut_ptr(), 1).is_null());
            assert_eq!(errno(), ERANGE);
            assert!(_wgetcwd(std::ptr::null_mut(), 1).is_null());
            assert_eq!(errno(), ERANGE);
            assert!(_wgetcwd(small.as_mut_ptr(), 0).is_null());
            assert_eq!(errno(), EINVAL);
            assert!(_wgetcwd(small.as_mut_ptr(), -1).is_null());
            assert_eq!(errno(), EINVAL);
        }
        assert_eq!(calls(), 2);
    }
}
