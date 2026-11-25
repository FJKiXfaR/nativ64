//! `ext-ms-win-rtcore-ntuser-message-ansi`: `PeekMessageA`, `DispatchMessageA`. Also under `user32`.

use crate::windowing::{self, Msg};

win32! {
    module "ext-ms-win-rtcore-ntuser-message-ansi" => "user32";
    Native fn PeekMessageA(msg: *mut Msg, hwnd: usize, min: u32, max: u32, flags: u32) -> i32 {
        windowing::peek_message(msg, hwnd, min, max, flags) as i32
    }
    Native fn DispatchMessageA(msg: *const Msg) -> isize {
        windowing::dispatch_message(msg)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registered() {
        for m in [
            "ext-ms-win-rtcore-ntuser-message-ansi-l1-1-0.dll",
            "user32.dll",
        ] {
            for n in ["PeekMessageA", "DispatchMessageA"] {
                assert!(crate::lookup(m, n).is_some(), "{m}!{n}");
            }
        }
    }

    #[test]
    fn empty_queue_and_null_dispatch() {
        let mut m = crate::windowing::Msg::default();
        unsafe {
            assert_eq!(super::PeekMessageA(&mut m, 0, 0, 0, 1), 0);
            assert_eq!(super::DispatchMessageA(&m), 0);
        }
    }
}
