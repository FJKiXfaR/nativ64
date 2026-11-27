//! Locale queries. The host reports a fixed `en-US` user, system and UI locale (LCID 0x0409,
//! geographic region `US`); nothing is read from the Linux environment yet. The values live in
//! the constants below so a later configuration layer has one place to override.

use crate::api_ms_win_core_errorhandling::set_last_error;
use std::ffi::c_void;

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

const ERROR_INVALID_FLAGS: u32 = 1004;
const LCMAP_LOWERCASE: u32 = 0x100;
const LCMAP_UPPERCASE: u32 = 0x200;
/// Only the tests name this: sort keys are unsupported, so it fails like any unknown flag.
#[cfg(test)]
const LCMAP_SORTKEY: u32 = 0x400;
const LCMAP_LINGUISTIC_CASING: u32 = 0x0100_0000;

/// The locale name a caller passed: NULL, "" (invariant), en-US, en or the system default.
unsafe fn known_locale(locale: *const u16) -> Option<()> {
    if locale.is_null() {
        return Some(());
    }
    let name = unsafe { read_wstr(locale) }?;
    (name.is_empty()
        || name.eq_ignore_ascii_case(DEFAULT_LOCALE_NAME)
        || name.eq_ignore_ascii_case("en")
        || name.eq_ignore_ascii_case(SYSTEM_DEFAULT_NAME))
    .then_some(())
}

/// Copies a locale string, returning the needed length (characters including the NUL) when the
/// buffer is too small or absent.
fn copy_locale_value(value: &str, buf: *mut u16, cch: i32) -> i32 {
    let units: Vec<u16> = value.encode_utf16().collect();
    let need = units.len() as i32 + 1;
    if buf.is_null() || cch == 0 {
        return need;
    }
    if cch < need {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    for (i, u) in units.iter().enumerate() {
        unsafe { buf.add(i).write_unaligned(*u) };
    }
    unsafe { buf.add(units.len()).write(0) };
    need
}

/// `GetLocaleInfoEx`'s known `LOCALE_*` values for the fixed en-US locale.
fn locale_value(lc_type: u32) -> Option<&'static str> {
    Some(match lc_type {
        0x0000_0001 => "0409",                    // LOCALE_ILANGUAGE
        0x0000_0002 => "English (United States)", // LOCALE_SLANGUAGE
        0x0000_0003 => "ENU",                     // LOCALE_SABBREVLANGNAME
        0x0000_0004 => "English",                 // LOCALE_SNATIVELANGNAME
        0x0000_0005 => "1",                       // LOCALE_ICOUNTRY
        0x0000_0006 => "United States",           // LOCALE_SCOUNTRY
        0x0000_0007 => "USA",                     // LOCALE_SABBREVCTRYNAME
        0x0000_0008 => "United States",           // LOCALE_SNATIVECTRYNAME
        0x0000_000b => "437",                     // LOCALE_IDEFAULTCODEPAGE
        0x0000_000c => ", ",                      // LOCALE_SLIST
        0x0000_000d => "0",                       // LOCALE_IMEASURE
        0x0000_000e => ".",                       // LOCALE_SDECIMAL
        0x0000_000f => ",",                       // LOCALE_STHOUSAND
        0x0000_0010 => "3;0",                     // LOCALE_SGROUPING
        0x0000_0011 => "2",                       // LOCALE_IDIGITS
        0x0000_0012 => "1",                       // LOCALE_ILZERO
        0x0000_0013 => "0123456789",              // LOCALE_SNATIVEDIGITS
        0x0000_0014 => "$",                       // LOCALE_SCURRENCY
        0x0000_0015 => "USD",                     // LOCALE_SINTLSYMBOL
        0x0000_0016 => ",",                       // LOCALE_SMONDTHOUS
        0x0000_0017 => ".",                       // LOCALE_SMONTDECIMAL
        0x0000_0018 => "3;0",                     // LOCALE_SMONGROUPING
        0x0000_0019 => "2",                       // LOCALE_ICURRDIGITS
        0x0000_001b => "0",                       // LOCALE_ICURRENCY
        0x0000_001c => "0",                       // LOCALE_INEGCURR
        0x0000_001d => "/",                       // LOCALE_SDATE
        0x0000_001e => ":",                       // LOCALE_STIME
        0x0000_001f => "M/d/yyyy",                // LOCALE_SSHORTDATE
        0x0000_0020 => "dddd, MMMM d, yyyy",      // LOCALE_SLONGDATE
        0x0000_0059 => "en",                      // LOCALE_SISO639LANGNAME
        0x0000_005a => "US",                      // LOCALE_SISO3166CTRYNAME
        0x0000_005c => DEFAULT_LOCALE_NAME,       // LOCALE_SNAME
        0x0000_1004 => "1252",                    // LOCALE_IDEFAULTANSICODEPAGE
        0x0000_1011 => "10000",                   // LOCALE_IDEFAULTMACCODEPAGE
        _ => return None,
    })
}

/// `GetCPInfo`: an 18-byte `CPINFO` (max char size, default char, lead bytes) for the code pages
/// the ANSI conversions know, plus 437 and 20127.
unsafe fn cp_info(code_page: u32, out: *mut u8) -> i32 {
    const SUPPORTED: [u32; 6] = [0, 1, 3, 437, 1252, 65001];
    if out.is_null() || !SUPPORTED.contains(&code_page) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let max_char = if code_page == 65001 { 4u32 } else { 1 };
    unsafe { (out as *mut u32).write_unaligned(max_char) };
    unsafe { out.add(4).write(b'?') };
    unsafe { out.add(5).write(0) };
    for i in 6..18 {
        unsafe { out.add(i).write(0) };
    }
    1
}

/// `LCMapStringEx`: case mapping only (sort keys are not modelled).
unsafe fn lc_map_string(
    locale: *const u16,
    flags: u32,
    src: *const u16,
    cch_src: i32,
    dst: *mut u16,
    cch_dst: i32,
) -> i32 {
    const KNOWN: u32 = LCMAP_LOWERCASE | LCMAP_UPPERCASE | LCMAP_LINGUISTIC_CASING;
    if unsafe { known_locale(locale) }.is_none() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if flags & !KNOWN != 0 {
        set_last_error(ERROR_INVALID_FLAGS);
        return 0;
    }
    let lower = flags & LCMAP_LOWERCASE != 0;
    let upper = flags & LCMAP_UPPERCASE != 0;
    if lower && upper {
        set_last_error(ERROR_INVALID_FLAGS);
        return 0;
    }
    if cch_src < -1 || (cch_src > 0 && src.is_null()) || (cch_dst > 0 && dst.is_null()) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let units: Vec<u16> = if cch_src < 0 {
        if src.is_null() {
            Vec::new()
        } else {
            let mut v = Vec::new();
            loop {
                let u = unsafe { src.add(v.len()).read_unaligned() };
                if u == 0 {
                    break;
                }
                v.push(u);
            }
            v
        }
    } else {
        (0..cch_src as usize)
            .map(|i| unsafe { src.add(i).read_unaligned() })
            .collect()
    };
    let mut mapped = String::new();
    for u in units {
        let c = char::from_u32(u32::from(u)).unwrap_or('\u{fffd}');
        if lower {
            mapped.extend(c.to_lowercase());
        } else if upper {
            mapped.extend(c.to_uppercase());
        } else {
            mapped.push(c);
        }
    }
    let out: Vec<u16> = mapped.encode_utf16().collect();
    if dst.is_null() || cch_dst == 0 {
        return out.len() as i32 + 1;
    }
    if cch_dst < out.len() as i32 {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    for (i, u) in out.iter().enumerate() {
        unsafe { dst.add(i).write_unaligned(*u) };
    }
    out.len() as i32
}

const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x100;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;
const FORMAT_MESSAGE_FROM_STRING: u32 = 0x400;
const FORMAT_MESSAGE_FROM_HMODULE: u32 = 0x800;
const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
const FORMAT_MESSAGE_ARGUMENT_ARRAY: u32 = 0x2000;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;

/// The text of a system message this process can name: the common Win32 errors, otherwise a
/// generic line with the number (there is no message table on the host).
fn system_message(message_id: u32) -> String {
    match message_id {
        0 => "The operation completed successfully.".to_string(),
        2 => "The system cannot find the file specified.".to_string(),
        3 => "The system cannot find the path specified.".to_string(),
        5 => "Access is denied.".to_string(),
        6 => "The handle is invalid.".to_string(),
        8 => "Not enough memory resources are available to process this command.".to_string(),
        87 => "The parameter is incorrect.".to_string(),
        122 => "The data area passed to a system call is too small.".to_string(),
        183 => "Cannot create a file when that file already exists.".to_string(),
        1004 => "Invalid flags.".to_string(),
        n => format!("Unknown error {n}."),
    }
}

/// Reads a NUL-terminated narrow string.
unsafe fn read_cstr(p: *const u8) -> String {
    let mut v = Vec::new();
    if p.is_null() {
        return String::new();
    }
    loop {
        let b = unsafe { *p.add(v.len()) };
        if b == 0 {
            break;
        }
        v.push(b);
    }
    String::from_utf8_lossy(&v).into_owned()
}

/// Expands `%1`..`%9` inserts (`%%` is a literal `%`); strings come from the va_list slots.
unsafe fn expand_inserts(template: &str, args: *const u64) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some(d) if d.is_ascii_digit() && *d != '0' => {
                let n = d.to_digit(10).unwrap() as usize;
                chars.next();
                if args.is_null() {
                    continue;
                }
                let p = unsafe { args.add(n - 1).read_unaligned() } as *const u16;
                if p.is_null() {
                    continue;
                }
                let mut v = Vec::new();
                loop {
                    let u = unsafe { p.add(v.len()).read_unaligned() };
                    if u == 0 {
                        break;
                    }
                    v.push(u);
                }
                out.push_str(&String::from_utf16_lossy(&v));
            }
            _ => out.push('%'),
        }
    }
    out
}

/// `FormatMessageW`/`FormatMessageA`: FROM_STRING with `%n` inserts, FROM_SYSTEM/FROM_HMODULE
/// with a small built-in message table, and ALLOCATE_BUFFER through `HeapAlloc`.
unsafe fn format_message(
    flags: u32,
    source: *const std::ffi::c_void,
    message_id: u32,
    buffer: *mut std::ffi::c_void,
    size: u32,
    args: *const u64,
    wide: bool,
) -> u32 {
    let from_string = flags & FORMAT_MESSAGE_FROM_STRING != 0;
    let from_module = flags & FORMAT_MESSAGE_FROM_HMODULE != 0;
    let from_system = flags & FORMAT_MESSAGE_FROM_SYSTEM != 0;
    if [from_string, from_module, from_system]
        .iter()
        .filter(|f| **f)
        .count()
        > 1
    {
        set_last_error(ERROR_INVALID_FLAGS);
        return 0;
    }
    let template = if from_string {
        if wide {
            unsafe { read_wstr(source as *const u16) }.unwrap_or_default()
        } else {
            unsafe { read_cstr(source as *const u8) }
        }
    } else if from_module || from_system {
        system_message(message_id)
    } else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let text = if flags & FORMAT_MESSAGE_IGNORE_INSERTS != 0 {
        // Inserts are left alone, but a doubled percent is still one percent.
        template.replace("%%", "%")
    } else if from_string && args.is_null() {
        template
    } else {
        unsafe { expand_inserts(&template, args) }
    };
    let _ = FORMAT_MESSAGE_ARGUMENT_ARRAY;
    if wide {
        let units: Vec<u16> = text.encode_utf16().collect();
        let need = units.len() as u32 + 1;
        if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
            let bytes = need as usize * 2;
            let p = unsafe { crate::api_ms_win_core_heap::heap_alloc(0, bytes, 0) } as *mut u16;
            if p.is_null() {
                set_last_error(ERROR_NOT_ENOUGH_MEMORY);
                return 0;
            }
            for (i, u) in units.iter().enumerate() {
                unsafe { p.add(i).write_unaligned(*u) };
            }
            unsafe { p.add(units.len()).write(0) };
            unsafe { (buffer as *mut *mut u16).write_unaligned(p) };
            return units.len() as u32;
        }
        if buffer.is_null() || size == 0 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        if size < need {
            set_last_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        let b = buffer as *mut u16;
        for (i, u) in units.iter().enumerate() {
            unsafe { b.add(i).write_unaligned(*u) };
        }
        unsafe { b.add(units.len()).write(0) };
        units.len() as u32
    } else {
        let mut bytes: Vec<u8> = text
            .chars()
            .map(|c| {
                if c as u32 <= 0xff {
                    c as u32 as u8
                } else {
                    b'?'
                }
            })
            .collect();
        let need = bytes.len() as u32 + 1;
        if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
            let p =
                unsafe { crate::api_ms_win_core_heap::heap_alloc(0, need as usize, 0) } as *mut u8;
            if p.is_null() {
                set_last_error(ERROR_NOT_ENOUGH_MEMORY);
                return 0;
            }
            bytes.push(0);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
            unsafe { (buffer as *mut *mut u8).write_unaligned(p) };
            return bytes.len() as u32 - 1;
        }
        if buffer.is_null() || size == 0 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        if size < need {
            set_last_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        bytes.push(0);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len()) };
        bytes.len() as u32 - 1
    }
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
    Native fn GetCPInfo(code_page: u32, out: *mut u8) -> i32 {
        unsafe { cp_info(code_page, out) }
    }
    Native fn GetLocaleInfoEx(locale: *const u16, lc_type: u32, buf: *mut u16, cch: i32) -> i32 {
        if unsafe { known_locale(locale) }.is_none() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let Some(value) = locale_value(lc_type) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        copy_locale_value(value, buf, cch)
    }
    Native fn LCMapStringEx(locale: *const u16, flags: u32, src: *const u16, cch_src: i32,
                            dst: *mut u16, cch_dst: i32, _version: *const c_void,
                            _reserved: *const c_void, _lparam: i64) -> i32 {
        unsafe { lc_map_string(locale, flags, src, cch_src, dst, cch_dst) }
    }


    Native fn FormatMessageW(flags: u32, source: *const c_void, message_id: u32, _lang: u32,
                             buffer: *mut c_void, size: u32, args: *const u64) -> u32 {
        unsafe { format_message(flags, source, message_id, buffer, size, args, true) }
    }
    Native fn FormatMessageA(flags: u32, source: *const c_void, message_id: u32, _lang: u32,
                             buffer: *mut c_void, size: u32, args: *const u64) -> u32 {
        unsafe { format_message(flags, source, message_id, buffer, size, args, false) }
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

    #[test]
    fn code_page_info() {
        unsafe {
            let mut info = [0xffu8; 18];
            assert_eq!(GetCPInfo(1252, info.as_mut_ptr()), 1);
            assert_eq!(u32::from_le_bytes(info[0..4].try_into().unwrap()), 1);
            assert_eq!(&info[4..6], b"?\0");
            assert_eq!(GetCPInfo(65001, info.as_mut_ptr()), 1);
            assert_eq!(u32::from_le_bytes(info[0..4].try_into().unwrap()), 4);
            assert_eq!(GetCPInfo(932, info.as_mut_ptr()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn locale_values() {
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        unsafe {
            let mut b = [0u16; 32];
            // LOCALE_SISO639LANGNAME, LOCALE_SISO3166CTRYNAME, LOCALE_SNAME, LOCALE_SLIST.
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x59, b.as_mut_ptr(), 32),
                3
            );
            assert_eq!(String::from_utf16_lossy(&b[..2]), "en");
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x5a, b.as_mut_ptr(), 32),
                3
            );
            assert_eq!(String::from_utf16_lossy(&b[..2]), "US");
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x5c, b.as_mut_ptr(), 32),
                6
            );
            assert_eq!(String::from_utf16_lossy(&b[..5]), "en-US");
            // Too small a buffer: the needed size comes back with ERROR_INSUFFICIENT_BUFFER.
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x5c, b.as_mut_ptr(), 2),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x5c, std::ptr::null_mut(), 0),
                6
            );
            // Unknown type and locale.
            assert_eq!(
                GetLocaleInfoEx(std::ptr::null(), 0x7fff, b.as_mut_ptr(), 32),
                0
            );
            let other = w("de-DE");
            assert_eq!(GetLocaleInfoEx(other.as_ptr(), 0x59, b.as_mut_ptr(), 32), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn case_mapping() {
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        unsafe {
            let src = w("AbC");
            let mut b = [0u16; 8];
            assert_eq!(
                LCMapStringEx(
                    std::ptr::null(),
                    LCMAP_LOWERCASE,
                    src.as_ptr(),
                    -1,
                    b.as_mut_ptr(),
                    8,
                    std::ptr::null(),
                    std::ptr::null(),
                    0
                ),
                3
            );
            assert_eq!(String::from_utf16_lossy(&b[..3]), "abc");
            assert_eq!(
                LCMapStringEx(
                    std::ptr::null(),
                    LCMAP_UPPERCASE,
                    src.as_ptr(),
                    -1,
                    b.as_mut_ptr(),
                    8,
                    std::ptr::null(),
                    std::ptr::null(),
                    0
                ),
                3
            );
            assert_eq!(String::from_utf16_lossy(&b[..3]), "ABC");
            // A needed-size query and the failure cases.
            assert_eq!(
                LCMapStringEx(
                    std::ptr::null(),
                    LCMAP_LOWERCASE,
                    src.as_ptr(),
                    -1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    0
                ),
                4
            );
            assert_eq!(
                LCMapStringEx(
                    std::ptr::null(),
                    LCMAP_LOWERCASE | LCMAP_UPPERCASE,
                    src.as_ptr(),
                    -1,
                    b.as_mut_ptr(),
                    8,
                    std::ptr::null(),
                    std::ptr::null(),
                    0
                ),
                0
            );
            assert_eq!(
                LCMapStringEx(
                    std::ptr::null(),
                    LCMAP_SORTKEY,
                    src.as_ptr(),
                    -1,
                    b.as_mut_ptr(),
                    8,
                    std::ptr::null(),
                    std::ptr::null(),
                    0
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_FLAGS);
        }
    }

    #[test]
    fn format_message_from_string() {
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let arg = w("world");
        unsafe {
            let template = w("Hello %1! 100%%");
            let args = [arg.as_ptr() as u64, 0u64];
            let mut out = [0u16; 64];
            // `%1` becomes "world" and `%%` a literal percent.
            let n = FormatMessageW(
                FORMAT_MESSAGE_FROM_STRING,
                template.as_ptr().cast(),
                0,
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                args.as_ptr(),
            );
            assert_eq!(n, 17);
            assert_eq!(String::from_utf16_lossy(&out[..17]), "Hello world! 100%");
            // IGNORE_INSERTS leaves the template alone.
            let n = FormatMessageW(
                FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_IGNORE_INSERTS,
                template.as_ptr().cast(),
                0,
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                std::ptr::null(),
            );
            assert_eq!(
                String::from_utf16_lossy(&out[..n as usize]),
                "Hello %1! 100%"
            );
            // ALLOCATE_BUFFER returns a LocalAlloc block.
            let mut p: *mut u16 = std::ptr::null_mut();
            let n = FormatMessageW(
                FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_ALLOCATE_BUFFER,
                template.as_ptr().cast(),
                0,
                0,
                (&mut p as *mut *mut u16).cast(),
                0,
                args.as_ptr(),
            );
            assert_eq!(n, 17);
            assert_eq!(
                String::from_utf16_lossy(std::slice::from_raw_parts(p, 17)),
                "Hello world! 100%"
            );
            assert_eq!(
                crate::api_ms_win_core_heap::LocalFree(p.cast()),
                std::ptr::null_mut()
            );
        }
    }

    #[test]
    fn format_message_system_and_errors() {
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        unsafe {
            let mut out = [0u16; 96];
            let n = FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                2,
                0,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                std::ptr::null(),
            );
            assert!(n > 0);
            assert!(String::from_utf16_lossy(&out[..n as usize]).contains("cannot find the file"));
            // A tiny buffer fails and says how, and the ANSI variant maps to bytes.
            assert_eq!(
                FormatMessageW(
                    FORMAT_MESSAGE_FROM_SYSTEM,
                    std::ptr::null(),
                    5,
                    0,
                    out.as_mut_ptr().cast(),
                    4,
                    std::ptr::null()
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            let mut a = [0u8; 64];
            let n = FormatMessageA(
                FORMAT_MESSAGE_FROM_SYSTEM,
                std::ptr::null(),
                5,
                0,
                a.as_mut_ptr().cast(),
                a.len() as u32,
                std::ptr::null(),
            );
            assert_eq!(n, 17);
            assert_eq!(&a[..n as usize], b"Access is denied.");
            // FROM_STRING with the ANSI template.
            let template = b"file %1".to_vec();
            let arg = w("x.txt");
            let args = [arg.as_ptr() as u64];
            let n = FormatMessageA(
                FORMAT_MESSAGE_FROM_STRING,
                template.as_ptr().cast(),
                0,
                0,
                a.as_mut_ptr().cast(),
                a.len() as u32,
                args.as_ptr(),
            );
            assert_eq!(&a[..n as usize], b"file x.txt");
        }
    }
}
