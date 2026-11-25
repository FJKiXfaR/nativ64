//! `ext-ms-win-rtcore-ntuser-message`: `TranslateMessage`. Also exported under `user32`.

use crate::windowing::{self, Msg};

win32! {
    module "ext-ms-win-rtcore-ntuser-message" => "user32";
    Native fn TranslateMessage(msg: *const Msg) -> i32 {
        windowing::translate_message(msg) as i32
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registered() {
        assert!(crate::lookup(
            "ext-ms-win-rtcore-ntuser-message-l1-1-0.dll",
            "TranslateMessage"
        )
        .is_some());
        assert!(crate::lookup("user32.dll", "TranslateMessage").is_some());
    }

    #[test]
    fn non_key_is_false() {
        let m = crate::windowing::Msg::new(0, 0x0200, 0, 0);
        assert_eq!(unsafe { super::TranslateMessage(&m) }, 0);
    }
}
