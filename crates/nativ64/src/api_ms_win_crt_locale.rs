//! `api-ms-win-crt-locale`: the "C" locale only.
//!
//! `setlocale` accepts the categories `LC_ALL` (0) to `LC_TIME` (5); a query (NULL), `"C"` and
//! `""` all answer `"C"` (deviation: `""`, the user's default locale on Windows, stays "C"
//! here); any other name fails with NULL. An invalid category sets `errno` to `EINVAL`, calls
//! the invalid parameter handler and returns NULL. `localeconv` returns a static `lconv` with
//! the UCRT layout (ten `char*`, eight `char`, eight `wchar_t*`); only the decimal point is
//! set. `__pctype_func` returns the UCRT's C locale `ctype` table (`unsigned short` flags, 257
//! entries: the pointer is at the entry of character 0, the entry for `EOF` precedes it).
//! The code pages are 0 and `___mb_cur_max_func` is 1.
//!
//! `_configthreadlocale` only records the per-thread mode (it changes nothing else: there is a
//! single, global, "C" locale); the default is `_DISABLE_PER_THREAD_LOCALE`. `_lock_locales`
//! and `_unlock_locales` are a global recursive lock.

use crate::api_ms_win_crt_runtime::invalid_parameter_err;
use std::cell::{Cell, UnsafeCell};
use std::ffi::{c_char, c_int};

const EINVAL: i32 = 22;
const LC_ALL: c_int = 0;
const LC_TIME: c_int = 5;

const ENABLE_PER_THREAD_LOCALE: c_int = 1;
const DISABLE_PER_THREAD_LOCALE: c_int = 2;

// ---------------------------------------------------------------------------------------------
// ctype table.

const UPPER: u16 = 0x1;
const LOWER: u16 = 0x2;
const DIGIT: u16 = 0x4;
const SPACE: u16 = 0x8;
const PUNCT: u16 = 0x10;
const CONTROL: u16 = 0x20;
const BLANK: u16 = 0x40;
const HEX: u16 = 0x80;
const ALPHA: u16 = 0x100;

const fn ctype_flags(c: u8) -> u16 {
    match c {
        0x09..=0x0D => CONTROL | SPACE,
        0x00..=0x1F | 0x7F => CONTROL,
        b' ' => SPACE | BLANK,
        b'0'..=b'9' => DIGIT | HEX,
        b'A'..=b'F' => UPPER | HEX | ALPHA,
        b'G'..=b'Z' => UPPER | ALPHA,
        b'a'..=b'f' => LOWER | HEX | ALPHA,
        b'g'..=b'z' => LOWER | ALPHA,
        0x21..=0x7E => PUNCT,
        _ => 0,
    }
}

/// Entry 0 is for `EOF` (-1), entries 1..=256 for the characters 0..=255.
static CTYPE: [u16; 257] = {
    let mut t = [0u16; 257];
    let mut i = 0;
    while i < 256 {
        t[i + 1] = ctype_flags(i as u8);
        i += 1;
    }
    t
};

// ---------------------------------------------------------------------------------------------
// lconv.

/// The UCRT `struct lconv`.
#[repr(C)]
pub struct Lconv {
    pub decimal_point: *const u8,
    pub thousands_sep: *const u8,
    pub grouping: *const u8,
    pub int_curr_symbol: *const u8,
    pub currency_symbol: *const u8,
    pub mon_decimal_point: *const u8,
    pub mon_thousands_sep: *const u8,
    pub mon_grouping: *const u8,
    pub positive_sign: *const u8,
    pub negative_sign: *const u8,
    pub int_frac_digits: i8,
    pub frac_digits: i8,
    pub p_cs_precedes: i8,
    pub p_sep_by_space: i8,
    pub n_cs_precedes: i8,
    pub n_sep_by_space: i8,
    pub p_sign_posn: i8,
    pub n_sign_posn: i8,
    pub w_decimal_point: *const u16,
    pub w_thousands_sep: *const u16,
    pub w_int_curr_symbol: *const u16,
    pub w_currency_symbol: *const u16,
    pub w_mon_decimal_point: *const u16,
    pub w_mon_thousands_sep: *const u16,
    pub w_positive_sign: *const u16,
    pub w_negative_sign: *const u16,
}

/// Holds immutable data with raw pointers into statics.
struct Sync_<T>(T);
unsafe impl<T> Sync for Sync_<T> {}

static NARROW_DOT: [u8; 2] = *b".\0";
static NARROW_EMPTY: [u8; 1] = [0];
static WIDE_DOT: [u16; 2] = [b'.' as u16, 0];
static WIDE_EMPTY: [u16; 1] = [0];

static LCONV: Sync_<Lconv> = Sync_(Lconv {
    decimal_point: NARROW_DOT.as_ptr(),
    thousands_sep: NARROW_EMPTY.as_ptr(),
    grouping: NARROW_EMPTY.as_ptr(),
    int_curr_symbol: NARROW_EMPTY.as_ptr(),
    currency_symbol: NARROW_EMPTY.as_ptr(),
    mon_decimal_point: NARROW_EMPTY.as_ptr(),
    mon_thousands_sep: NARROW_EMPTY.as_ptr(),
    mon_grouping: NARROW_EMPTY.as_ptr(),
    positive_sign: NARROW_EMPTY.as_ptr(),
    negative_sign: NARROW_EMPTY.as_ptr(),
    int_frac_digits: 127,
    frac_digits: 127,
    p_cs_precedes: 127,
    p_sep_by_space: 127,
    n_cs_precedes: 127,
    n_sep_by_space: 127,
    p_sign_posn: 127,
    n_sign_posn: 127,
    w_decimal_point: WIDE_DOT.as_ptr(),
    w_thousands_sep: WIDE_EMPTY.as_ptr(),
    w_int_curr_symbol: WIDE_EMPTY.as_ptr(),
    w_currency_symbol: WIDE_EMPTY.as_ptr(),
    w_mon_decimal_point: WIDE_EMPTY.as_ptr(),
    w_mon_thousands_sep: WIDE_EMPTY.as_ptr(),
    w_positive_sign: WIDE_EMPTY.as_ptr(),
    w_negative_sign: WIDE_EMPTY.as_ptr(),
});

static C_NAME: [u8; 2] = *b"C\0";
static LOCALE_NAMES: Sync_<[*const u16; 6]> = Sync_([std::ptr::null(); 6]);

// ---------------------------------------------------------------------------------------------

thread_local! {
    static THREAD_LOCALE_MODE: Cell<c_int> = const { Cell::new(DISABLE_PER_THREAD_LOCALE) };
}

/// A process-wide recursive mutex.
struct RecursiveLock(UnsafeCell<libc::pthread_mutex_t>);
unsafe impl Sync for RecursiveLock {}

static LOCALE_LOCK: RecursiveLock = RecursiveLock(UnsafeCell::new(
    libc::PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP,
));

win32! {
    module "api-ms-win-crt-locale" => "ucrtbase";
    Native fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char {
        if !(LC_ALL..=LC_TIME).contains(&category) {
            invalid_parameter_err(EINVAL);
            return std::ptr::null_mut();
        }
        if !locale.is_null() {
            let name = std::ffi::CStr::from_ptr(locale).to_bytes();
            if !name.is_empty() && name != b"C" {
                return std::ptr::null_mut();
            }
        }
        C_NAME.as_ptr() as *mut c_char
    }
    Native fn localeconv() -> *mut Lconv {
        &LCONV.0 as *const Lconv as *mut Lconv
    }
    Native fn ___lc_codepage_func() -> u32 {
        0
    }
    Native fn ___lc_collate_cp_func() -> u32 {
        0
    }
    Native fn ___lc_locale_name_func() -> *mut *const u16 {
        LOCALE_NAMES.0.as_ptr() as *mut *const u16
    }
    Native fn ___mb_cur_max_func() -> c_int {
        1
    }
    Native fn __pctype_func() -> *const u16 {
        CTYPE.as_ptr().add(1)
    }
    Native fn _configthreadlocale(mode: c_int) -> c_int {
        let prev = THREAD_LOCALE_MODE.with(|m| m.get());
        match mode {
            0 => {}
            ENABLE_PER_THREAD_LOCALE | DISABLE_PER_THREAD_LOCALE => {
                THREAD_LOCALE_MODE.with(|m| m.set(mode));
            }
            // `_ENABLE_PER_THREAD_LOCALE` combined with `_GLOBAL` (0x10) or `_NEW` (0x100).
            0x11 | 0x101 => THREAD_LOCALE_MODE.with(|m| m.set(ENABLE_PER_THREAD_LOCALE)),
            _ => {
                invalid_parameter_err(EINVAL);
                return -1;
            }
        }
        prev
    }
    Native fn _lock_locales() {
        libc::pthread_mutex_lock(LOCALE_LOCK.0.get());
    }
    Native fn _unlock_locales() {
        libc::pthread_mutex_unlock(LOCALE_LOCK.0.get());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{_set_errno, errno, test_support::*};
    use std::ffi::CStr;

    const NAMES: &str = "setlocale localeconv ___lc_codepage_func ___lc_collate_cp_func ___lc_locale_name_func ___mb_cur_max_func __pctype_func _configthreadlocale _lock_locales _unlock_locales";

    #[test]
    fn registered_both_names() {
        assert_eq!(NAMES.split(' ').count(), 10);
        for n in NAMES.split(' ') {
            assert!(
                crate::lookup("api-ms-win-crt-locale-l1-1-0.dll", n).is_some(),
                "{n}"
            );
            assert!(crate::lookup("ucrtbase", n).is_some(), "{n}");
        }
    }

    #[test]
    fn setlocale_accepts_only_c() {
        unsafe {
            for cat in LC_ALL..=LC_TIME {
                for q in [std::ptr::null(), c"C".as_ptr(), c"".as_ptr()] {
                    let r = setlocale(cat, q);
                    assert_eq!(CStr::from_ptr(r).to_bytes(), b"C");
                }
            }
            assert!(setlocale(LC_ALL, c"fr-FR".as_ptr()).is_null());
            assert!(setlocale(LC_ALL, c"c".as_ptr()).is_null());
            assert!(setlocale(4, c"Polish".as_ptr()).is_null());
            assert_eq!(
                CStr::from_ptr(setlocale(LC_ALL, std::ptr::null())).to_bytes(),
                b"C"
            );
        }
    }

    #[test]
    fn setlocale_bad_category() {
        let _g = install_counting_handler();
        unsafe {
            for cat in [20, -1, 6] {
                _set_errno(0);
                assert!(setlocale(cat, c"C".as_ptr()).is_null());
                assert_eq!(errno(), EINVAL);
            }
        }
        assert_eq!(calls(), 3);
    }

    #[test]
    fn localeconv_layout_and_values() {
        assert_eq!(std::mem::size_of::<Lconv>(), 10 * 8 + 8 + 8 * 8);
        assert_eq!(std::mem::offset_of!(Lconv, int_frac_digits), 80);
        assert_eq!(std::mem::offset_of!(Lconv, w_decimal_point), 88);
        unsafe {
            let l = &*localeconv();
            assert_eq!(
                CStr::from_ptr(l.decimal_point as *const c_char).to_bytes(),
                b"."
            );
            for p in [
                l.thousands_sep,
                l.grouping,
                l.int_curr_symbol,
                l.currency_symbol,
                l.mon_decimal_point,
                l.mon_thousands_sep,
                l.mon_grouping,
                l.positive_sign,
                l.negative_sign,
            ] {
                assert_eq!(*p, 0);
            }
            assert_eq!(
                [
                    l.int_frac_digits,
                    l.frac_digits,
                    l.p_cs_precedes,
                    l.p_sep_by_space,
                    l.n_cs_precedes,
                    l.n_sep_by_space,
                    l.p_sign_posn,
                    l.n_sign_posn
                ],
                [127; 8]
            );
            assert_eq!(
                (*l.w_decimal_point, *l.w_decimal_point.add(1)),
                (b'.' as u16, 0)
            );
            assert_eq!(*l.w_negative_sign, 0);
            assert_eq!(localeconv(), localeconv());
        }
    }

    #[test]
    fn lc_helpers() {
        unsafe {
            assert_eq!(
                (
                    ___lc_codepage_func(),
                    ___lc_collate_cp_func(),
                    ___mb_cur_max_func()
                ),
                (0, 0, 1)
            );
            let names = ___lc_locale_name_func();
            for i in 0..6 {
                assert!((*names.add(i)).is_null());
            }
        }
    }

    #[test]
    fn ctype_table() {
        unsafe {
            let t = __pctype_func();
            assert_eq!(*t.offset(-1), 0, "EOF");
            let f = |c: u8| *t.add(c as usize);
            assert_eq!(f(0), CONTROL);
            assert_eq!(f(b'\t'), 0x28);
            assert_eq!(f(b'\r'), 0x28);
            assert_eq!(f(b' '), 0x48);
            assert_eq!(f(b'0'), 0x84);
            assert_eq!(f(b'A'), 0x181);
            assert_eq!(f(b'G'), 0x101);
            assert_eq!(f(b'a'), 0x182);
            assert_eq!(f(b'z'), 0x102);
            assert_eq!(f(b'!'), PUNCT);
            assert_eq!(f(b'~'), PUNCT);
            assert_eq!(f(0x7F), CONTROL);
            assert_eq!(f(0x80), 0);
            assert_eq!(f(0xFF), 0);
            for c in 0u8..=127 {
                let a = f(c) & ALPHA != 0;
                let up = f(c) & UPPER != 0;
                let lo = f(c) & LOWER != 0;
                let dg = f(c) & DIGIT != 0;
                let hx = f(c) & HEX != 0;
                let sp = f(c) & SPACE != 0;
                assert_eq!(a, c.is_ascii_alphabetic());
                assert_eq!(up, c.is_ascii_uppercase());
                assert_eq!(lo, c.is_ascii_lowercase());
                assert_eq!(dg, c.is_ascii_digit());
                assert_eq!(hx, c.is_ascii_hexdigit());
                assert_eq!(sp, matches!(c, b' ' | 9..=13));
                assert_eq!(f(c) & PUNCT != 0, c.is_ascii_punctuation());
                assert_eq!(f(c) & CONTROL != 0, c.is_ascii_control());
            }
        }
    }

    #[test]
    fn configthreadlocale_modes() {
        let _g = install_counting_handler();
        std::thread::spawn(|| unsafe {
            assert_eq!(_configthreadlocale(0), DISABLE_PER_THREAD_LOCALE);
            assert_eq!(
                _configthreadlocale(ENABLE_PER_THREAD_LOCALE),
                DISABLE_PER_THREAD_LOCALE
            );
            assert_eq!(_configthreadlocale(0), ENABLE_PER_THREAD_LOCALE);
            _set_errno(0);
            assert_eq!(_configthreadlocale(-1), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(_configthreadlocale(7), -1);
            // Failed calls change nothing.
            assert_eq!(
                _configthreadlocale(DISABLE_PER_THREAD_LOCALE),
                ENABLE_PER_THREAD_LOCALE
            );
            assert_eq!(_configthreadlocale(0), DISABLE_PER_THREAD_LOCALE);
            // The mode is per thread.
            _configthreadlocale(ENABLE_PER_THREAD_LOCALE);
            std::thread::spawn(|| assert_eq!(_configthreadlocale(0), DISABLE_PER_THREAD_LOCALE))
                .join()
                .unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn locale_lock_is_recursive_and_exclusive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        unsafe {
            _lock_locales();
            _lock_locales();
            let got = Arc::new(AtomicBool::new(false));
            let g2 = got.clone();
            let h = std::thread::spawn(move || {
                _lock_locales();
                g2.store(true, Ordering::SeqCst);
                _unlock_locales();
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!got.load(Ordering::SeqCst), "held by this thread");
            _unlock_locales();
            std::thread::sleep(std::time::Duration::from_millis(30));
            assert!(!got.load(Ordering::SeqCst), "still held once");
            _unlock_locales();
            h.join().unwrap();
            assert!(got.load(Ordering::SeqCst));
        }
    }
}
