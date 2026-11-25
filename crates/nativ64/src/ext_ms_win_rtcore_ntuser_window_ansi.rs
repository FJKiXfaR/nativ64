//! `ext-ms-win-rtcore-ntuser-window-ansi`: class registration, window creation and the default
//! window procedure (ANSI). Also exported under `user32`. See `windowing.rs` for the semantics.

use crate::windowing::{self, WndClassExA};

win32! {
    module "ext-ms-win-rtcore-ntuser-window-ansi" => "user32";
    Native fn RegisterClassExA(wc: *const WndClassExA) -> u16 {
        windowing::register_class(wc)
    }
    Native fn CreateWindowExA(ex_style: u32, class: *const u8, name: *const u8, style: u32,
        x: i32, y: i32, w: i32, h: i32, parent: usize, menu: usize, instance: usize,
        param: usize) -> usize {
        windowing::create_window(ex_style, class, name, style, x, y, w, h, parent, menu, instance, param)
    }
    Native fn DefWindowProcA(hwnd: usize, msg: u32, wparam: usize, lparam: isize) -> isize {
        windowing::def_window_proc(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registered() {
        for m in [
            "ext-ms-win-rtcore-ntuser-window-ansi-l1-1-0.dll",
            "USER32.dll",
        ] {
            for n in ["RegisterClassExA", "CreateWindowExA", "DefWindowProcA"] {
                assert!(crate::lookup(m, n).is_some(), "{m}!{n}");
            }
        }
    }

    #[test]
    fn through_exports() {
        let cls = c"w_export";
        let wc = crate::windowing::WndClassExA {
            cb_size: 80,
            style: 0,
            wnd_proc: super::DefWindowProcA as *const () as usize,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: 0,
            h_icon: 0,
            h_cursor: 0,
            hbr_background: 0,
            menu_name: std::ptr::null(),
            class_name: cls.as_ptr().cast(),
            h_icon_sm: 0,
        };
        unsafe {
            assert!(super::RegisterClassExA(&wc) >= 0xC000);
            let h = super::CreateWindowExA(
                0,
                cls.as_ptr().cast(),
                std::ptr::null(),
                0,
                0,
                0,
                10,
                10,
                0,
                0,
                0,
                0,
            );
            assert_ne!(h, 0);
            assert_eq!(
                super::DefWindowProcA(h, crate::windowing::WM_NCCREATE, 0, 0),
                1
            );
        }
    }
}
