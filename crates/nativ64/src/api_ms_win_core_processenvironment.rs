//! The command line and the environment the loader runs with.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::api_ms_win_crt_runtime::full_command_line;
use std::ffi::c_char;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

/// Copies `value` (UTF-16) into `buf` the way the environment getters do.
fn copy_wide(value: &str, buf: *mut u16, size: u32) -> u32 {
    let units: Vec<u16> = value.encode_utf16().collect();
    let need = units.len() as u32 + 1;
    if buf.is_null() || size < need {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return need;
    }
    for (i, u) in units.iter().enumerate() {
        unsafe { buf.add(i).write_unaligned(*u) };
    }
    unsafe { buf.add(units.len()).write(0) };
    units.len() as u32
}

/// Copies `value` (bytes) into `buf` the way the ANSI environment getters do.
fn copy_narrow(value: &[u8], buf: *mut u8, size: u32) -> u32 {
    let need = value.len() as u32 + 1;
    if buf.is_null() || size < need {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return need;
    }
    unsafe { std::ptr::copy_nonoverlapping(value.as_ptr(), buf, value.len()) };
    unsafe { buf.add(value.len()).write(0) };
    value.len() as u32
}

win32! {
    module "api-ms-win-core-processenvironment" => "kernel32";
    // The whole command line, program name included, as the CRT startup sees it.
    Native fn GetCommandLineA() -> *const c_char {
        full_command_line().0
    }
    Native fn GetCommandLineW() -> *const u16 {
        full_command_line().1
    }
    Native fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, size: u32) -> u32 {
        let Some(n) = crate::handles::name_w(name) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        match std::env::var_os(&n) {
            Some(v) => copy_wide(&v.to_string_lossy(), buf, size),
            None => {
                set_last_error(ERROR_ENVVAR_NOT_FOUND);
                0
            }
        }
    }
    Native fn GetEnvironmentVariableA(name: *const u8, buf: *mut u8, size: u32) -> u32 {
        let Some(n) = crate::handles::name_a(name) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        match std::env::var_os(&n) {
            Some(v) => copy_narrow(v.to_string_lossy().as_bytes(), buf, size),
            None => {
                set_last_error(ERROR_ENVVAR_NOT_FOUND);
                0
            }
        }
    }
    Native fn SetEnvironmentVariableA(name: *const u8, value: *const u8) -> i32 {
        let Some(n) = crate::handles::name_a(name) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        if value.is_null() {
            std::env::remove_var(&n);
            return 1;
        }
        let Some(v) = crate::handles::name_a(value) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        std::env::set_var(&n, &v);
        1
    }
    Native fn GetStdHandle(which: u32) -> usize {
        crate::api_ms_win_core_console::GetStdHandle(which)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;

    #[test]
    fn narrow_and_wide_command_line_agree() {
        unsafe {
            let a = std::ffi::CStr::from_ptr(GetCommandLineA())
                .to_str()
                .unwrap();
            let w = GetCommandLineW();
            let mut v = Vec::new();
            let mut i = 0;
            while *w.add(i) != 0 {
                v.push(*w.add(i));
                i += 1;
            }
            assert_eq!(String::from_utf16(&v).unwrap(), a);
            assert_eq!(GetCommandLineA(), GetCommandLineA());
        }
    }

    #[test]
    fn environment_round_trip() {
        let name = b"NATIV64_TEST_VAR\0";
        let value = b"a title value\0";
        unsafe {
            assert_eq!(SetEnvironmentVariableA(name.as_ptr(), value.as_ptr()), 1);
            let mut a = [0u8; 32];
            let n = GetEnvironmentVariableA(name.as_ptr(), a.as_mut_ptr(), a.len() as u32);
            assert_eq!(n as usize, value.len() - 1);
            assert_eq!(&a[..n as usize], &value[..value.len() - 1]);
            let mut w = [0u16; 32];
            let n = GetEnvironmentVariableW(
                "NATIV64_TEST_VAR"
                    .encode_utf16()
                    .chain([0])
                    .collect::<Vec<_>>()
                    .as_ptr(),
                w.as_mut_ptr(),
                w.len() as u32,
            );
            assert_eq!(String::from_utf16_lossy(&w[..n as usize]), "a title value");
            // Removing with a NULL value.
            assert_eq!(SetEnvironmentVariableA(name.as_ptr(), std::ptr::null()), 1);
            assert_eq!(
                GetEnvironmentVariableA(name.as_ptr(), a.as_mut_ptr(), a.len() as u32),
                0
            );
            assert_eq!(GetLastError(), ERROR_ENVVAR_NOT_FOUND);
            // A buffer that is too small reports the needed size.
            SetEnvironmentVariableA(name.as_ptr(), value.as_ptr());
            let mut small = [0u8; 4];
            let need = GetEnvironmentVariableA(name.as_ptr(), small.as_mut_ptr(), 4);
            assert_eq!(need as usize, value.len());
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            SetEnvironmentVariableA(name.as_ptr(), std::ptr::null());
        }
    }
}
