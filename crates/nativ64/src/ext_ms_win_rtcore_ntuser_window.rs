//! `ext-ms-win-rtcore-ntuser-window`: `ShowWindow`. Also exported under `user32`.

use crate::windowing;

win32! {
    module "ext-ms-win-rtcore-ntuser-window" => "user32";
    Native fn ShowWindow(hwnd: usize, cmd: i32) -> i32 {
        windowing::show_window(hwnd, cmd) as i32
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registered() {
        assert!(
            crate::lookup("ext-ms-win-rtcore-ntuser-window-l1-1-0.dll", "ShowWindow").is_some()
        );
        assert!(crate::lookup("user32.dll", "ShowWindow").is_some());
    }

    #[test]
    fn bad_handle() {
        assert_eq!(unsafe { super::ShowWindow(0x9999, 5) }, 0);
    }
}
