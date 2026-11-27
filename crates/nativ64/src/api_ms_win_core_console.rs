//! The console: standard handles and the buffer info a console program reads.
//!
//! The loader has no Windows console: the standard handles are the three pseudo values Windows
//! uses (`-10`, `-11`, `-12`), `WriteConsoleW` writes the text to the loader's stdout/stderr (so
//! a title's console output lands in the run log), and `GetConsoleWindow` has no window.

use crate::api_ms_win_core_errorhandling::set_last_error;

const ERROR_INVALID_HANDLE: u32 = 6;

/// `STD_INPUT_HANDLE`, `STD_OUTPUT_HANDLE` and `STD_ERROR_HANDLE` as the handle values.
pub const STD_INPUT_HANDLE: usize = -10isize as usize;
pub const STD_OUTPUT_HANDLE: usize = -11isize as usize;
pub const STD_ERROR_HANDLE: usize = -12isize as usize;

/// True when `h` is one of the three standard handles.
pub fn is_std_handle(h: usize) -> bool {
    matches!(h, STD_INPUT_HANDLE | STD_OUTPUT_HANDLE | STD_ERROR_HANDLE)
}

/// Write UTF-8 to the host stream a standard handle names.
pub fn write_std(handle: usize, bytes: &[u8]) -> bool {
    use std::io::Write;
    let ok = if handle == STD_ERROR_HANDLE {
        std::io::stderr().write_all(bytes).is_ok()
    } else {
        std::io::stdout().write_all(bytes).is_ok()
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    ok
}

win32! {
    module "api-ms-win-core-console" => "kernel32";
    Native fn GetStdHandle(which: u32) -> usize {
        match which as i32 {
            -10 => STD_INPUT_HANDLE,
            -11 => STD_OUTPUT_HANDLE,
            -12 => STD_ERROR_HANDLE,
            _ => {
                set_last_error(ERROR_INVALID_HANDLE);
                0
            }
        }
    }
    Native fn GetConsoleWindow() -> usize {
        0
    }
    // CONSOLE_SCREEN_BUFFER_INFO (22 bytes): an 80x25 buffer, the cursor at home, the default
    // grey-on-black attributes and the whole buffer in the window.
    Native fn GetConsoleScreenBufferInfo(h: usize, out: *mut u8) -> i32 {
        if out.is_null() || !is_std_handle(h) {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        let put16 = |at: usize, v: u16| unsafe { (out.add(at) as *mut u16).write_unaligned(v) };
        put16(0, 80); // dwSize.X
        put16(2, 25); // dwSize.Y
        put16(4, 0); // dwCursorPosition.X
        put16(6, 0); // dwCursorPosition.Y
        put16(8, 0x7); // wAttributes
        put16(10, 0); // srWindow.Left
        put16(12, 0); // srWindow.Top
        put16(14, 79); // srWindow.Right
        put16(16, 24); // srWindow.Bottom
        put16(18, 80); // dwMaximumWindowSize.X
        put16(20, 25); // dwMaximumWindowSize.Y
        1
    }
    Native fn WriteConsoleW(h: usize, text: *const u16, len: u32, written: *mut u32, _reserved: *const u8) -> i32 {
        if !is_std_handle(h) || (text.is_null() && len != 0) {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        let units: Vec<u16> = (0..len as usize)
            .map(|i| unsafe { text.add(i).read_unaligned() })
            .collect();
        let s = String::from_utf16_lossy(&units).replace('\n', "\r\n");
        if !write_std(h, s.as_bytes()) {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        if !written.is_null() {
            unsafe { written.write_unaligned(len) };
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_handles_and_buffer_info() {
        unsafe {
            assert_eq!(GetStdHandle(-10i32 as u32), STD_INPUT_HANDLE);
            assert_eq!(GetStdHandle(-11i32 as u32), STD_OUTPUT_HANDLE);
            assert_eq!(GetStdHandle(-12i32 as u32), STD_ERROR_HANDLE);
            assert_eq!(GetStdHandle(7), 0);
            assert_eq!(GetConsoleWindow(), 0);
            let mut info = [0xffu8; 22];
            assert_eq!(
                GetConsoleScreenBufferInfo(STD_OUTPUT_HANDLE, info.as_mut_ptr()),
                1
            );
            assert_eq!(u16::from_le_bytes(info[0..2].try_into().unwrap()), 80);
            assert_eq!(u16::from_le_bytes(info[2..4].try_into().unwrap()), 25);
            assert_eq!(u16::from_le_bytes(info[8..10].try_into().unwrap()), 0x7);
            assert_eq!(GetConsoleScreenBufferInfo(0x1234, info.as_mut_ptr()), 0);
        }
    }

    #[test]
    fn write_console_counts_the_units() {
        let text: Vec<u16> = "hello\n".encode_utf16().collect();
        let mut written = 0u32;
        unsafe {
            assert_eq!(
                WriteConsoleW(
                    STD_OUTPUT_HANDLE,
                    text.as_ptr(),
                    text.len() as u32,
                    &mut written,
                    std::ptr::null()
                ),
                1
            );
        }
        assert_eq!(written, text.len() as u32);
    }
}
