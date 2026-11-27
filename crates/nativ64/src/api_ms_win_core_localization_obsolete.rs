//! `api-ms-win-core-localization-obsolete`: the language-id getters (`kernel32` forwards).
//!
//! The host reports a fixed en-US system and user language (`LANGID` 0x0409), the same value the
//! other locale getters return.

use crate::api_ms_win_core_localization::DEFAULT_LCID;

win32! {
    module "api-ms-win-core-localization-obsolete" => "kernel32";
    Native fn GetUserDefaultUILanguage() -> u16 {
        DEFAULT_LCID as u16
    }
    Native fn GetSystemDefaultUILanguage() -> u16 {
        DEFAULT_LCID as u16
    }
    Native fn GetUserDefaultLangID() -> u16 {
        DEFAULT_LCID as u16
    }
    Native fn GetSystemDefaultLangID() -> u16 {
        DEFAULT_LCID as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_are_en_us() {
        unsafe {
            assert_eq!(GetUserDefaultUILanguage(), 0x0409);
            assert_eq!(GetSystemDefaultUILanguage(), 0x0409);
            assert_eq!(GetUserDefaultLangID(), 0x0409);
            assert_eq!(GetSystemDefaultLangID(), 0x0409);
        }
    }
}
