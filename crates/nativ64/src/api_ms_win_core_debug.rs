//! `api-ms-win-core-debug`: no debugger is ever attached, so debug strings normally go
//! nowhere. `NATIV64_DEBUG_STRING=1` echoes them to stderr (`nativ64: debug string: ...`),
//! which is how a title's own fatal-error text is read without attaching a debugger.

use std::ffi::{c_char, c_void, CStr};

/// Whether debug strings are echoed (`NATIV64_DEBUG_STRING`, set to anything but "0"/empty).
fn echo_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("NATIV64_DEBUG_STRING").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

fn echo(bytes: &[u8]) {
    if !echo_enabled() {
        return;
    }
    let mut line = Vec::with_capacity(bytes.len() + 24);
    line.extend_from_slice(b"nativ64: debug string: ");
    line.extend_from_slice(bytes);
    line.push(b'\n');
    eprint!("{}", String::from_utf8_lossy(&line));
}

/// Decodes UTF-16 to a host string; a lone surrogate becomes U+FFFD, as the host runtime does.
fn utf16_string(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// # Safety
/// `text` is NULL or a NUL-terminated ANSI string.
unsafe fn output_a(text: *const u8) {
    if text.is_null() {
        return;
    }
    echo(unsafe { CStr::from_ptr(text as *const c_char) }.to_bytes());
}

/// # Safety
/// `text` is NULL or a NUL-terminated UTF-16 string.
unsafe fn output_w(text: *const u16) {
    if text.is_null() {
        return;
    }
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    echo(utf16_string(units).as_bytes());
}

win32! {
    module "api-ms-win-core-debug" => "kernel32";
    // Without an attached debugger Windows drops the string (and raises nothing); the option
    // only mirrors it to this host's stderr.
    Native fn OutputDebugStringA(text: *const u8) { unsafe { output_a(text) } }
    Native fn OutputDebugStringW(text: *const u16) { unsafe { output_w(text) } }
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

    #[test]
    fn utf16_decodes_like_the_host() {
        assert_eq!(utf16_string(&[]), "");
        assert_eq!(utf16_string(&[u16::from(b'h'), u16::from(b'i')]), "hi");
        assert_eq!(utf16_string(&[0xd83d, 0xde00]), "\u{1f600}");
        assert_eq!(
            utf16_string(&[0xd83d]),
            char::REPLACEMENT_CHARACTER.to_string()
        );
        assert_eq!(
            utf16_string(&[0xde00]),
            char::REPLACEMENT_CHARACTER.to_string()
        );
    }

    #[test]
    fn null_and_empty_strings_are_harmless() {
        let empty16 = [0u16];
        unsafe {
            output_a(std::ptr::null());
            output_w(std::ptr::null());
            output_a(c"".as_ptr().cast());
            output_w(empty16.as_ptr());
        }
    }
}
