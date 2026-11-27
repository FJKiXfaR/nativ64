//! `api-ms-win-core-string-obsolete`: the `lstr*` helpers (`kernel32` forwards).
//!
//! `lstrlenW` counts UTF-16 units. The two comparisons are ordinal and case-folded with the
//! host's Unicode tables; Windows' `lstrcmp*` are locale-aware, so the answers agree on ASCII
//! and may differ where the locale's collation would order two characters differently.

use crate::api_ms_win_core_errorhandling::set_last_error;

const ERROR_INVALID_PARAMETER: u32 = 87;

/// The length of a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` is NULL or NUL-terminated.
unsafe fn wlen(p: *const u16) -> usize {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    n
}

fn fold(c: u16, ignore_case: bool) -> char {
    let ch = char::from_u32(u32::from(c)).unwrap_or('\u{fffd}');
    if ignore_case {
        ch.to_lowercase().next().unwrap_or(ch)
    } else {
        ch
    }
}

/// Ordinal comparison of two NUL-terminated UTF-16 strings: -1, 0 or 1.
///
/// # Safety
/// Both pointers are non-NULL and NUL-terminated.
unsafe fn compare(a: *const u16, b: *const u16, ignore_case: bool) -> i32 {
    let (la, lb) = (unsafe { wlen(a) }, unsafe { wlen(b) });
    for i in 0..la.min(lb) {
        let (x, y) = (
            fold(unsafe { *a.add(i) }, ignore_case),
            fold(unsafe { *b.add(i) }, ignore_case),
        );
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    match la.cmp(&lb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

win32! {
    module "api-ms-win-core-string-obsolete" => "kernel32";
    Native fn lstrlenW(s: *const u16) -> i32 {
        if s.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        unsafe { wlen(s) as i32 }
    }
    Native fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
        if a.is_null() || b.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        unsafe { compare(a, b, false) }
    }
    Native fn lstrcmpiW(a: *const u16, b: *const u16) -> i32 {
        if a.is_null() || b.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        unsafe { compare(a, b, true) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn lengths_and_ordinal_comparisons() {
        unsafe {
            assert_eq!(lstrlenW(w("hello").as_ptr()), 5);
            assert_eq!(lstrlenW(w("").as_ptr()), 0);
            assert_eq!(lstrlenW(std::ptr::null()), 0);
            assert_eq!(lstrcmpW(w("abc").as_ptr(), w("abc").as_ptr()), 0);
            assert_eq!(lstrcmpW(w("abc").as_ptr(), w("abd").as_ptr()), -1);
            assert_eq!(lstrcmpW(w("ab").as_ptr(), w("abc").as_ptr()), -1);
            assert_eq!(lstrcmpW(w("b").as_ptr(), w("a").as_ptr()), 1);
            assert_eq!(lstrcmpiW(w("AbC").as_ptr(), w("aBc").as_ptr()), 0);
            assert_eq!(lstrcmpiW(w("Zebra").as_ptr(), w("apple").as_ptr()), 1);
            assert_eq!(lstrcmpiW(w("é").as_ptr(), w("É").as_ptr()), 0);
        }
    }

    #[test]
    fn registered_under_kernel32() {
        for n in ["lstrlenW", "lstrcmpW", "lstrcmpiW"] {
            assert!(
                lookup("api-ms-win-core-string-obsolete-l1-1-0.dll", n).is_some(),
                "{n}"
            );
            assert!(lookup("kernel32.dll", n).is_some(), "{n}");
        }
    }
}
