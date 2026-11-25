//! Locale queries. The host reports a fixed `en-US` user, system and UI locale (LCID 0x0409,
//! geographic region `US`); nothing is read from the Linux environment yet. The values live in
//! the constants below so a later configuration layer has one place to override.

use crate::api_ms_win_core_errorhandling::set_last_error;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// The user/system default locale name and its LCID.
pub(crate) const DEFAULT_LOCALE_NAME: &str = "en-US";
pub(crate) const DEFAULT_LCID: u32 = 0x0409;
/// The invariant locale (empty name).
const INVARIANT_LCID: u32 = 0x007f;
/// ISO 3166-1 alpha-2 code of the default geographic region.
const DEFAULT_GEO_NAME: &str = "US";
/// `LOCALE_NAME_SYSTEM_DEFAULT`.
const SYSTEM_DEFAULT_NAME: &str = "!x-sys-default-locale";

/// Copies `s` plus a NUL into `buf` (`len` UTF-16 units) the way the locale getters do:
/// `len == 0` only reports the size, a short buffer fails with `ERROR_INSUFFICIENT_BUFFER`.
/// Returns the size in characters, NUL included, or 0.
unsafe fn copy_name(s: &str, buf: *mut u16, len: i32) -> i32 {
    let need = s.encode_utf16().count() + 1;
    if len < 0 || (len > 0 && buf.is_null()) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if len == 0 {
        return need as i32;
    }
    if (len as usize) < need {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    for (i, u) in s.encode_utf16().enumerate() {
        buf.add(i).write_unaligned(u);
    }
    buf.add(need - 1).write_unaligned(0);
    need as i32
}

/// Reads a NUL-terminated UTF-16 string (bounded, so a missing terminator cannot run away).
unsafe fn read_wstr(p: *const u16) -> Option<String> {
    let mut v = Vec::new();
    for i in 0..256 {
        let u = p.add(i).read_unaligned();
        if u == 0 {
            return String::from_utf16(&v).ok();
        }
        v.push(u);
    }
    None
}

win32! {
    module "api-ms-win-core-localization" => "kernel32";
    // The user's locale name, e.g. `en-US`.
    Native fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32 {
        copy_name(DEFAULT_LOCALE_NAME, name, len)
    }
    // The user's geographic region as a two-letter code, e.g. `US`.
    Native fn GetUserDefaultGeoName(geo_name: *mut u16, count: i32) -> i32 {
        if count != 0 && geo_name.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        copy_name(DEFAULT_GEO_NAME, geo_name, count)
    }
    // NULL is the user default, `!x-sys-default-locale` the system default, "" the invariant.
    // Only en-US (and its neutral `en`) are known; anything else is an invalid parameter.
    Native fn LocaleNameToLCID(name: *const u16, _flags: u32) -> u32 {
        if name.is_null() {
            return DEFAULT_LCID;
        }
        let Some(n) = read_wstr(name) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        if n.is_empty() {
            INVARIANT_LCID
        } else if n.eq_ignore_ascii_case(DEFAULT_LOCALE_NAME)
            || n.eq_ignore_ascii_case(SYSTEM_DEFAULT_NAME)
            || n.eq_ignore_ascii_case("en")
        {
            DEFAULT_LCID
        } else {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn locale_name() {
        unsafe {
            let mut b = [0xffffu16; 16];
            assert_eq!(GetUserDefaultLocaleName(b.as_mut_ptr(), 16), 6);
            assert_eq!(&b[..6], &w("en-US")[..]);
            assert_eq!(GetUserDefaultLocaleName(std::ptr::null_mut(), 0), 6);
            assert_eq!(GetUserDefaultLocaleName(b.as_mut_ptr(), 5), 0);
            assert_eq!(GetLastError(), 122);
            assert_eq!(GetUserDefaultLocaleName(std::ptr::null_mut(), 5), 0);
            assert_eq!(GetLastError(), 87);
        }
    }

    #[test]
    fn geo_name() {
        unsafe {
            let mut b = [0xffffu16; 4];
            assert_eq!(GetUserDefaultGeoName(b.as_mut_ptr(), 4), 3);
            assert_eq!(&b[..3], &w("US")[..]);
            assert_eq!(GetUserDefaultGeoName(std::ptr::null_mut(), 0), 3);
            assert_eq!(GetUserDefaultGeoName(b.as_mut_ptr(), 2), 0);
            assert_eq!(GetLastError(), 122);
            assert_eq!(GetUserDefaultGeoName(std::ptr::null_mut(), 3), 0);
            assert_eq!(GetLastError(), 87);
        }
    }

    #[test]
    fn name_to_lcid() {
        unsafe {
            assert_eq!(LocaleNameToLCID(std::ptr::null(), 0), 0x0409);
            assert_eq!(LocaleNameToLCID(w("EN-us").as_ptr(), 0), 0x0409);
            assert_eq!(
                LocaleNameToLCID(w("!x-sys-default-locale").as_ptr(), 0),
                0x0409
            );
            assert_eq!(LocaleNameToLCID(w("").as_ptr(), 0), 0x7f);
            assert_eq!(LocaleNameToLCID(w("xx-YY").as_ptr(), 0), 0);
            assert_eq!(GetLastError(), 87);
        }
    }
}
