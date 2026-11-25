//! `api-ms-win-core-registry`: a read-only registry that is empty.
//!
//! Only the four predefined root keys exist (`HKEY_CLASSES_ROOT`, `HKEY_CURRENT_USER`,
//! `HKEY_LOCAL_MACHINE`, `HKEY_USERS`); they hold no subkey and no value. Opening a subkey or
//! querying a value therefore reports `ERROR_FILE_NOT_FOUND`, which is what a program sees on a
//! machine without the key. Registry functions return the error code itself (they do not set
//! the last error).
//!
//! Functions are registered under their API set and under `advapi32`, which forwards to them.

use std::ffi::c_void;

const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_INVALID_HANDLE: i32 = 6;
const ERROR_INVALID_PARAMETER: i32 = 87;

const HKEY_CLASSES_ROOT: usize = 0x8000_0000;
const HKEY_USERS: usize = 0x8000_0003;

fn is_root(h: usize) -> bool {
    // Predefined handles are sign-extended 32-bit values on 64-bit Windows.
    h as i64 == (h as u32 as i32) as i64
        && (HKEY_CLASSES_ROOT..=HKEY_USERS).contains(&(h & 0xFFFF_FFFF))
}

/// True when the (possibly NULL) wide string is NULL or empty.
unsafe fn empty(p: *const u16) -> bool {
    p.is_null() || *p == 0
}

win32! {
    module "api-ms-win-core-registry" => "advapi32";
    // `result` receives the same predefined key for a NULL or empty subkey; any other subkey
    // does not exist.
    Native fn RegOpenKeyW(key: usize, subkey: *const u16, result: *mut usize) -> i32 {
        if result.is_null() {
            return ERROR_INVALID_PARAMETER;
        }
        if !is_root(key) {
            return ERROR_INVALID_HANDLE;
        }
        if empty(subkey) {
            *result = key;
            return ERROR_SUCCESS;
        }
        ERROR_FILE_NOT_FOUND
    }
    // Predefined keys are never really opened, so closing one is a no-op.
    Native fn RegCloseKey(key: usize) -> i32 {
        if is_root(key) { ERROR_SUCCESS } else { ERROR_INVALID_HANDLE }
    }
    Native fn RegQueryValueExW(key: usize, _name: *const u16, _reserved: *mut u32, _ty: *mut u32,
                               _data: *mut u8, _size: *mut u32) -> i32 {
        if !is_root(key) {
            return ERROR_INVALID_HANDLE;
        }
        ERROR_FILE_NOT_FOUND
    }
    Native fn RegGetValueW(key: usize, subkey: *const u16, _name: *const u16, _flags: u32,
                           _ty: *mut u32, _data: *mut c_void, _size: *mut u32) -> i32 {
        if !is_root(key) {
            return ERROR_INVALID_HANDLE;
        }
        let _ = subkey;
        ERROR_FILE_NOT_FOUND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }
    const HKLM: usize = 0xFFFF_FFFF_8000_0002;

    #[test]
    fn registered_both_names() {
        for n in [
            "RegOpenKeyW",
            "RegCloseKey",
            "RegQueryValueExW",
            "RegGetValueW",
        ] {
            assert!(crate::lookup("ADVAPI32.dll", n).is_some(), "{n}");
            assert!(
                crate::lookup("api-ms-win-core-registry-l1-1-0.dll", n).is_some(),
                "{n}"
            );
        }
    }

    #[test]
    fn roots_are_empty() {
        unsafe {
            let mut k = 0usize;
            assert_eq!(
                RegOpenKeyW(HKLM, w("Software\\X").as_ptr(), &mut k),
                ERROR_FILE_NOT_FOUND
            );
            assert_eq!(k, 0);
            assert_eq!(RegOpenKeyW(HKLM, std::ptr::null(), &mut k), 0);
            assert_eq!(k, HKLM);
            assert_eq!(RegOpenKeyW(HKLM, w("").as_ptr(), &mut k), 0);
            assert_eq!(RegCloseKey(k), 0);
            let n = w("V");
            assert_eq!(
                RegQueryValueExW(
                    HKLM,
                    n.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                ERROR_FILE_NOT_FOUND
            );
            assert_eq!(
                RegGetValueW(
                    HKLM,
                    w("S").as_ptr(),
                    n.as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                ERROR_FILE_NOT_FOUND
            );
        }
    }

    #[test]
    fn errors() {
        unsafe {
            let mut k = 0usize;
            assert_eq!(
                RegOpenKeyW(HKLM, std::ptr::null(), std::ptr::null_mut()),
                ERROR_INVALID_PARAMETER
            );
            assert_eq!(
                RegOpenKeyW(0x1234, std::ptr::null(), &mut k),
                ERROR_INVALID_HANDLE
            );
            assert_eq!(RegCloseKey(0x1234), ERROR_INVALID_HANDLE);
            assert_eq!(RegCloseKey(0), ERROR_INVALID_HANDLE);
            assert_eq!(
                RegQueryValueExW(
                    0x1234,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                ERROR_INVALID_HANDLE
            );
        }
    }
}
