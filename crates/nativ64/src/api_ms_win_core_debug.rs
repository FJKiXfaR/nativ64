//! `api-ms-win-core-debug`: no debugger is ever attached, so debug strings go nowhere.

use std::ffi::c_void;

win32! {
    module "api-ms-win-core-debug" => "kernel32";
    // Without an attached debugger Windows drops the string (and raises nothing).
    Native fn OutputDebugStringA(_text: *const u8) {}
    Native fn OutputDebugStringW(_text: *const u16) {}
    Native fn IsDebuggerPresent() -> i32 {
        0
    }
    Native fn CheckRemoteDebuggerPresent(_process: *mut c_void, present: *mut i32) -> i32 {
        if present.is_null() {
            return 0;
        }
        *present = 0;
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;

    #[test]
    fn no_debugger_is_attached() {
        unsafe {
            OutputDebugStringA(c"hello".as_ptr().cast());
            OutputDebugStringW(std::ptr::null());
            assert_eq!(IsDebuggerPresent(), 0);
            let mut present = 7;
            assert_eq!(
                CheckRemoteDebuggerPresent(std::ptr::null_mut(), &mut present),
                1
            );
            assert_eq!(present, 0);
            assert_eq!(
                CheckRemoteDebuggerPresent(std::ptr::null_mut(), std::ptr::null_mut()),
                0
            );
        }
        assert!(lookup("api-ms-win-core-debug-l1-1-0.dll", "OutputDebugStringA").is_some());
        assert!(lookup("kernel32.dll", "IsDebuggerPresent").is_some());
    }
}
