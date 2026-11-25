//! `api-ms-win-crt-convert`: the UCRT string/number conversion functions.
//!
//! ABI notes (MSVC x64): `long` is 32 bits, so `atol`, `strtol`, `strtoul` and `wcstoul` use
//! 32-bit ranges and saturate there; `wchar_t`/`wint_t` are 16 bits; `size_t` is 64 bits.
//! Integer parsing skips C-locale whitespace, accepts a sign, and for base 0/16 a `0x` prefix
//! (a bare `0x` is no conversion). An invalid base or no digits yields 0 with `*endptr = nptr`.
//! Unsigned parsers negate like C (`"-1"` gives the type's maximum); out of range gives the
//! type's maximum, negated for a `-` sign (so `"-4294967296"` gives 1).
//!
//! `strtod`/`strtof`/`atof` delegate to the host libc (glibc) `strtod`/`strtof`: the process
//! never calls `setlocale`, so it parses in the "C" locale, and it handles decimal, hex floats
//! (`0x1.8p1`), `inf`/`infinity` and `nan` like the UCRT.
//!
//! Locale: "C" only; in it `btowc`/`wcstombs*` map code units 0..=255 one to one and reject
//! larger wide characters. `errno` (see `api_ms_win_crt_runtime`): `ERANGE` on `strto*` overflow
//! (and `strtod`/`strtof` overflow or underflow to zero), `EILSEQ` on an unconvertible wide
//! character, `EINVAL` plus the invalid parameter handler for a bad `strto*` base, a bad `_itoa`
//! radix or a bad `_s` argument. `_itoa` with a bad radix writes an empty string. Known gap:
//! `atof`/`wcstod`-style paths do not set `errno`, and a NULL string is not diagnosed.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use crate::api_ms_win_crt_string::is_space_unit;
use std::ffi::{c_char, c_int};

const EINVAL: i32 = 22;
const ERANGE: i32 = 34;
const EILSEQ: i32 = 42;
const STRUNCATE: i32 = 80;
const TRUNCATE: usize = usize::MAX;
const WEOF: u16 = 0xFFFF;

trait Unit: Copy + Into<u32> {}
impl Unit for u8 {}
impl Unit for u16 {}

/// Result of scanning an integer: magnitude, sign, overflow, and where scanning stopped.
struct Scan<T> {
    neg: bool,
    mag: u64,
    overflow: bool,
    end: *const T,
}

fn digit_val(c: u32) -> u32 {
    match c {
        0x30..=0x39 => c - 0x30,
        0x41..=0x5A => c - 0x41 + 10,
        0x61..=0x7A => c - 0x61 + 10,
        _ => 99,
    }
}

unsafe fn scan<T: Unit>(s: *const T, base: i32) -> Scan<T> {
    let none = Scan {
        neg: false,
        mag: 0,
        overflow: false,
        end: s,
    };
    if base == 1 || !(0..=36).contains(&base) {
        invalid_parameter_err(EINVAL);
        return none;
    }
    let at = |p: *const T, i: usize| -> u32 { (*p.add(i)).into() };
    let mut p = s;
    while is_space_unit(at(p, 0)) {
        p = p.add(1);
    }
    let mut neg = false;
    if at(p, 0) == '-' as u32 || at(p, 0) == '+' as u32 {
        neg = at(p, 0) == '-' as u32;
        p = p.add(1);
    }
    let mut base = base as u32;
    if (base == 0 || base == 16) && at(p, 0) == '0' as u32 && (at(p, 1) | 0x20) == 'x' as u32 {
        // Behaviour per Wine's dlls/msvcrt/tests/string.c (run on Windows): the prefix is
        // consumed even with no digit after it, which then counts as no conversion at all.
        p = p.add(2);
        base = 16;
    } else if base == 0 {
        base = if at(p, 0) == '0' as u32 { 8 } else { 10 };
    }
    let (mut mag, mut overflow, mut any) = (0u64, false, false);
    loop {
        let d = digit_val(at(p, 0));
        if d >= base {
            break;
        }
        any = true;
        match mag
            .checked_mul(base as u64)
            .and_then(|m| m.checked_add(d as u64))
        {
            Some(m) => mag = m,
            None => overflow = true,
        }
        p = p.add(1);
    }
    if !any {
        return none;
    }
    Scan {
        neg,
        mag,
        overflow,
        end: p,
    }
}

unsafe fn set_end<T>(end: *mut *mut T, p: *const T) {
    if !end.is_null() {
        *end = p as *mut T;
    }
}

/// Signed conversion saturating to `[min, max]`.
unsafe fn strto_i<T: Unit>(s: *const T, end: *mut *mut T, base: i32, min: i64, max: i64) -> i64 {
    let sc = scan(s, base);
    set_end(end, sc.end);
    if sc.neg {
        if sc.overflow || sc.mag > min.unsigned_abs() {
            set_errno(ERANGE);
            min
        } else {
            (sc.mag as i64).wrapping_neg()
        }
    } else if sc.overflow || sc.mag > max as u64 {
        set_errno(ERANGE);
        max
    } else {
        sc.mag as i64
    }
}

/// Unsigned conversion: overflow saturates to `max`, then a `-` sign negates at the type's width.
unsafe fn strto_u<T: Unit>(s: *const T, end: *mut *mut T, base: i32, max: u64) -> u64 {
    let sc = scan(s, base);
    set_end(end, sc.end);
    let mag = if sc.overflow || sc.mag > max {
        set_errno(ERANGE);
        max
    } else {
        sc.mag
    };
    if sc.neg {
        mag.wrapping_neg() & max
    } else {
        mag
    }
}

/// Digits of `value` in `radix`, with a `-` for negative values only in radix 10.
fn format_radix(value: i32, radix: i32) -> Option<Vec<u8>> {
    if !(2..=36).contains(&radix) {
        return None;
    }
    let (neg, mut v) = if radix == 10 && value < 0 {
        (true, (value as i64).unsigned_abs())
    } else {
        (false, value as u32 as u64)
    };
    let mut out = vec![];
    loop {
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(v % radix as u64) as usize]);
        v /= radix as u64;
        if v == 0 {
            break;
        }
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    Some(out)
}

unsafe fn clear_host_errno() {
    *libc::__errno_location() = 0;
}

/// After a glibc `strtod`/`strtof`: the UCRT sets `ERANGE` on overflow and on underflow to
/// zero, but not for a denormal result (glibc also flags those).
unsafe fn host_range_error(infinite: bool, zero: bool) {
    if *libc::__errno_location() == ERANGE && (infinite || zero) {
        set_errno(ERANGE);
    }
}

unsafe fn wc_to_byte(c: u16) -> Option<u8> {
    u8::try_from(c).ok()
}

win32! {
    module "api-ms-win-crt-convert" => "ucrtbase";
    // Returns `buf`; the caller guarantees the buffer is large enough (as in C).
    Native fn _itoa(value: i32, buf: *mut c_char, radix: i32) -> *mut c_char {
        if buf.is_null() {
            invalid_parameter_err(EINVAL);
            return buf;
        }
        match format_radix(value, radix) {
            Some(d) => {
                std::ptr::copy_nonoverlapping(d.as_ptr() as *const c_char, buf, d.len());
                *buf.add(d.len()) = 0;
            }
            None => {
                *buf = 0;
                invalid_parameter_err(EINVAL);
            }
        }
        buf
    }
    Native fn _itow_s(value: i32, buf: *mut u16, size: usize, radix: i32) -> i32 {
        if buf.is_null() || size == 0 {
            return invalid_parameter_err(EINVAL);
        }
        let Some(d) = format_radix(value, radix) else {
            *buf = 0;
            return invalid_parameter_err(EINVAL);
        };
        if d.len() + 1 > size {
            *buf = 0;
            return invalid_parameter_err(ERANGE);
        }
        for (i, b) in d.iter().enumerate() {
            *buf.add(i) = *b as u16;
        }
        *buf.add(d.len()) = 0;
        0
    }
    Native fn _strtoi64(s: *const c_char, end: *mut *mut c_char, base: i32) -> i64 {
        strto_i(s as *const u8, end as *mut *mut u8, base, i64::MIN, i64::MAX)
    }
    Native fn _strtoui64(s: *const c_char, end: *mut *mut c_char, base: i32) -> u64 {
        strto_u(s as *const u8, end as *mut *mut u8, base, u64::MAX)
    }
    Native fn _wtoi(s: *const u16) -> i32 {
        strto_i(s, std::ptr::null_mut(), 10, i32::MIN as i64, i32::MAX as i64) as i32
    }
    Native fn atof(s: *const c_char) -> f64 { libc::strtod(s, std::ptr::null_mut()) }
    Native fn atoi(s: *const c_char) -> i32 {
        strto_i(s as *const u8, std::ptr::null_mut(), 10, i32::MIN as i64, i32::MAX as i64) as i32
    }
    Native fn atol(s: *const c_char) -> i32 {
        strto_i(s as *const u8, std::ptr::null_mut(), 10, i32::MIN as i64, i32::MAX as i64) as i32
    }
    Native fn atoll(s: *const c_char) -> i64 {
        strto_i(s as *const u8, std::ptr::null_mut(), 10, i64::MIN, i64::MAX)
    }
    Native fn btowc(c: c_int) -> u16 {
        if (0..=255).contains(&c) { c as u16 } else { WEOF }
    }
    Native fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
        clear_host_errno();
        let r = libc::strtod(s, end);
        host_range_error(r.is_infinite(), r == 0.0);
        r
    }
    Native fn strtof(s: *const c_char, end: *mut *mut c_char) -> f32 {
        clear_host_errno();
        let r = libc::strtof(s, end);
        host_range_error(r.is_infinite(), r == 0.0);
        r
    }
    Native fn strtol(s: *const c_char, end: *mut *mut c_char, base: i32) -> i32 {
        strto_i(s as *const u8, end as *mut *mut u8, base, i32::MIN as i64, i32::MAX as i64) as i32
    }
    Native fn strtoll(s: *const c_char, end: *mut *mut c_char, base: i32) -> i64 {
        strto_i(s as *const u8, end as *mut *mut u8, base, i64::MIN, i64::MAX)
    }
    Native fn strtoul(s: *const c_char, end: *mut *mut c_char, base: i32) -> u32 {
        strto_u(s as *const u8, end as *mut *mut u8, base, u32::MAX as u64) as u32
    }
    Native fn strtoull(s: *const c_char, end: *mut *mut c_char, base: i32) -> u64 {
        strto_u(s as *const u8, end as *mut *mut u8, base, u64::MAX)
    }
    // Returns the number of bytes written (without the NUL), or `(size_t)-1` on a wide
    // character above 255. With a NULL `dst`, only counts.
    Native fn wcstombs(dst: *mut c_char, src: *const u16, n: usize) -> usize {
        if src.is_null() {
            return usize::MAX;
        }
        let mut i = 0;
        loop {
            let c = *src.add(i);
            if dst.is_null() || i < n {
                if c == 0 {
                    if !dst.is_null() {
                        *dst.add(i) = 0;
                    }
                    return i;
                }
                let Some(b) = wc_to_byte(c) else {
                    set_errno(EILSEQ);
                    return usize::MAX;
                };
                if !dst.is_null() {
                    *dst.add(i) = b as c_char;
                }
                i += 1;
            } else {
                return i;
            }
        }
    }
    Native fn wcstombs_s(
        ret: *mut usize, dst: *mut c_char, size: usize, src: *const u16, count: usize
    ) -> i32 {
        if !ret.is_null() {
            *ret = 0;
        }
        if src.is_null() || (dst.is_null() != (size == 0)) {
            if !dst.is_null() && size > 0 {
                *dst = 0;
            }
            return invalid_parameter_err(EINVAL);
        }
        if dst.is_null() {
            // Size query: bytes needed including the NUL. As in the UCRT's "C" locale, the
            // characters are only counted, not checked (Wine's msvcrt/tests/string.c).
            let mut n = 0;
            while *src.add(n) != 0 && n < count {
                n += 1;
            }
            if !ret.is_null() {
                *ret = n + 1;
            }
            return 0;
        }
        let limit = if count == TRUNCATE { size - 1 } else { count.min(size - 1) };
        let mut n = 0;
        while n < limit && *src.add(n) != 0 {
            let Some(b) = wc_to_byte(*src.add(n)) else {
                *dst = 0;
                set_errno(EILSEQ);
                return EILSEQ;
            };
            *dst.add(n) = b as c_char;
            n += 1;
        }
        // Did the source have more characters we were not allowed or able to copy?
        let more = *src.add(n) != 0 && n < count;
        if more && count != TRUNCATE {
            *dst = 0;
            return invalid_parameter_err(ERANGE);
        }
        *dst.add(n) = 0;
        if !ret.is_null() {
            *ret = n + 1;
        }
        if more { STRUNCATE } else { 0 }
    }
    Native fn wcstoul(s: *const u16, end: *mut *mut u16, base: i32) -> u32 {
        strto_u(s, end, base, u32::MAX as u64) as u32
    }
    Native fn wcstoull(s: *const u16, end: *mut *mut u16, base: i32) -> u64 {
        strto_u(s, end, base, u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn c(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }
    fn w(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }
    fn p(v: &[u8]) -> *const c_char {
        v.as_ptr() as *const c_char
    }
    fn st(v: &[u8]) -> String {
        CStr::from_bytes_until_nul(v)
            .unwrap()
            .to_str()
            .unwrap()
            .into()
    }
    fn ws(v: &[u16]) -> String {
        let n = v.iter().position(|&x| x == 0).unwrap();
        String::from_utf16(&v[..n]).unwrap()
    }
    /// Parses with `f`, returning (value, bytes consumed).
    unsafe fn tl(s: &str, base: i32) -> (i32, usize) {
        let b = c(s);
        let mut e: *mut c_char = std::ptr::null_mut();
        let v = strtol(p(&b), &mut e, base);
        (v, e as usize - p(&b) as usize)
    }

    #[test]
    fn registry_calls() {
        let get = |n: &str| {
            crate::lookup("api-ms-win-crt-convert-l1-1-0.dll", n)
                .unwrap()
                .addr
        };
        unsafe {
            let f: unsafe extern "win64" fn(*const c_char) -> i32 =
                std::mem::transmute(get("atoi"));
            assert_eq!(f(p(&c("  -42x"))), -42);
            let f: unsafe extern "win64" fn(*const c_char, *mut *mut c_char) -> f64 =
                std::mem::transmute(get("strtod"));
            assert_eq!(f(p(&c("2.5e1")), std::ptr::null_mut()), 25.0);
            let f: unsafe extern "win64" fn(*const c_char, *mut *mut c_char) -> f32 =
                std::mem::transmute(get("strtof"));
            assert_eq!(f(p(&c("0.5")), std::ptr::null_mut()), 0.5);
            let f: unsafe extern "win64" fn(c_int) -> u16 = std::mem::transmute(get("btowc"));
            assert_eq!(f(65), 65);
        }
        assert!(crate::lookup("ucrtbase", "wcstoull").is_some());
    }

    #[test]
    fn every_name_registered() {
        let names = "_itoa _itow_s _strtoi64 _strtoui64 _wtoi atof atoi atol atoll btowc strtod strtof strtol strtoll strtoul strtoull wcstombs wcstombs_s wcstoul wcstoull";
        assert_eq!(names.split(' ').count(), 20);
        for n in names.split(' ') {
            assert!(crate::lookup("api-ms-win-crt-convert", n).is_some(), "{n}");
        }
    }

    #[test]
    fn strtol_basics() {
        unsafe {
            assert_eq!(tl("123", 10), (123, 3));
            assert_eq!(tl("  \t-123abc", 10), (-123, 7));
            assert_eq!(tl("+7", 10), (7, 2));
            assert_eq!(tl("0x1F", 16), (31, 4));
            assert_eq!(tl("0x1F", 0), (31, 4));
            assert_eq!(tl("1F", 16), (31, 2));
            assert_eq!(tl("017", 0), (15, 3));
            assert_eq!(tl("0", 0), (0, 1));
            assert_eq!(tl("zz", 36), (1295, 2));
            assert_eq!(tl("1012", 2), (5, 3));
            // a bare "0x" prefix is no conversion (Wine's msvcrt/tests/string.c, run on Windows)
            assert_eq!(tl("0xg", 16), (0, 0));
            assert_eq!(tl("-0x", 0), (0, 0));
            // no digits: endptr = start, even after whitespace/sign
            assert_eq!(tl("  -x", 10), (0, 0));
            assert_eq!(tl("", 10), (0, 0));
            // invalid bases
            assert_eq!(tl("12", 1), (0, 0));
            assert_eq!(tl("12", 37), (0, 0));
            assert_eq!(tl("12", -2), (0, 0));
            // null endptr is allowed
            assert_eq!(strtol(p(&c("5")), std::ptr::null_mut(), 10), 5);
        }
    }

    #[test]
    fn strtol_saturates_at_32_bits() {
        unsafe {
            assert_eq!(tl("2147483647", 10), (i32::MAX, 10));
            assert_eq!(tl("2147483648", 10), (i32::MAX, 10));
            assert_eq!(tl("-2147483648", 10), (i32::MIN, 11));
            assert_eq!(tl("-2147483649", 10), (i32::MIN, 11));
            assert_eq!(tl("99999999999999999999999", 10), (i32::MAX, 23));
            assert_eq!(atol(p(&c("4294967296"))), i32::MAX);
            assert_eq!(atoi(p(&c("-99999999999"))), i32::MIN);
        }
    }

    #[test]
    fn unsigned_conversions() {
        unsafe {
            let mut e: *mut c_char = std::ptr::null_mut();
            let b = c("4294967295 ");
            assert_eq!(strtoul(p(&b), &mut e, 10), u32::MAX);
            assert_eq!(e as usize - p(&b) as usize, 10);
            assert_eq!(
                strtoul(p(&c("4294967296")), std::ptr::null_mut(), 10),
                u32::MAX
            );
            assert_eq!(strtoul(p(&c("-1")), std::ptr::null_mut(), 10), u32::MAX);
            // Overflow saturates first, then the sign negates: -(2^32) gives 1.
            assert_eq!(strtoul(p(&c("-4294967295")), std::ptr::null_mut(), 10), 1);
            assert_eq!(strtoul(p(&c("-4294967296")), std::ptr::null_mut(), 10), 1);
            assert_eq!(
                strtoull(p(&c("-99999999999999999999")), std::ptr::null_mut(), 10),
                1
            );
            assert_eq!(strtoul(p(&c("-2")), std::ptr::null_mut(), 10), u32::MAX - 1);
            assert_eq!(strtoul(p(&c("ff")), std::ptr::null_mut(), 16), 255);
            assert_eq!(
                strtoull(p(&c("18446744073709551615")), std::ptr::null_mut(), 10),
                u64::MAX
            );
            assert_eq!(
                strtoull(p(&c("18446744073709551616")), std::ptr::null_mut(), 10),
                u64::MAX
            );
            assert_eq!(strtoull(p(&c("-1")), std::ptr::null_mut(), 10), u64::MAX);
            assert_eq!(
                strtoull(p(&c("0xdeadbeefcafe")), std::ptr::null_mut(), 0),
                0xdeadbeefcafe
            );
            assert_eq!(_strtoui64(p(&c("123")), std::ptr::null_mut(), 10), 123);
            assert_eq!(_strtoui64(p(&c("zzz")), &mut e, 10), 0);
            assert_eq!(strtoul(p(&c("")), std::ptr::null_mut(), 10), 0);
            // wide
            let b = w("€12");
            let mut e: *mut u16 = std::ptr::null_mut();
            assert_eq!(wcstoul(b.as_ptr(), &mut e, 10), 0);
            assert_eq!(e as *const u16, b.as_ptr());
            let b = w("  0x7fffffff!");
            assert_eq!(wcstoul(b.as_ptr(), &mut e, 16), 0x7fffffff);
            assert_eq!(e.offset_from(b.as_ptr()), 12);
            assert_eq!(
                wcstoul(w("4294967296").as_ptr(), std::ptr::null_mut(), 10),
                u32::MAX
            );
            assert_eq!(
                wcstoull(w("-1").as_ptr(), std::ptr::null_mut(), 10),
                u64::MAX
            );
            assert_eq!(
                wcstoull(w("1234567890123").as_ptr(), std::ptr::null_mut(), 10),
                1234567890123
            );
        }
    }

    #[test]
    fn signed_64() {
        unsafe {
            let n = std::ptr::null_mut();
            assert_eq!(_strtoi64(p(&c("-9223372036854775808")), n, 10), i64::MIN);
            assert_eq!(_strtoi64(p(&c("-9223372036854775809")), n, 10), i64::MIN);
            assert_eq!(_strtoi64(p(&c("9223372036854775808")), n, 10), i64::MAX);
            assert_eq!(strtoll(p(&c("-0x10")), n, 16), -16);
            assert_eq!(atoll(p(&c("9000000000"))), 9_000_000_000);
            assert_eq!(atoll(p(&c("abc"))), 0);
            assert_eq!(_wtoi(w(" 77x").as_ptr()), 77);
            assert_eq!(_wtoi(w("-3000000000").as_ptr()), i32::MIN);
            assert_eq!(_wtoi(w("").as_ptr()), 0);
        }
    }

    #[test]
    fn floats() {
        unsafe {
            let b = c("  -1.5e2xyz");
            let mut e: *mut c_char = std::ptr::null_mut();
            assert_eq!(strtod(p(&b), &mut e), -150.0);
            assert_eq!(e as usize - p(&b) as usize, 8);
            assert_eq!(strtod(p(&c("0x1.8p1")), std::ptr::null_mut()), 3.0);
            assert_eq!(strtod(p(&c("inf")), std::ptr::null_mut()), f64::INFINITY);
            assert_eq!(
                strtod(p(&c("-Infinity")), std::ptr::null_mut()),
                f64::NEG_INFINITY
            );
            assert!(strtod(p(&c("nan")), std::ptr::null_mut()).is_nan());
            assert_eq!(strtod(p(&c("1e999")), std::ptr::null_mut()), f64::INFINITY);
            let b = c("abc");
            assert_eq!(strtod(p(&b), &mut e), 0.0);
            assert_eq!(e as *const c_char, p(&b));
            assert_eq!(strtof(p(&c("1.25")), std::ptr::null_mut()), 1.25);
            assert_eq!(strtof(p(&c("1e50")), std::ptr::null_mut()), f32::INFINITY);
            assert_eq!(strtof(p(&c("0x1p-2")), std::ptr::null_mut()), 0.25);
            assert_eq!(atof(p(&c(" 1.23456"))), 1.23456);
            assert_eq!(atof(p(&c(""))), 0.0);
        }
    }

    #[test]
    fn itoa_family() {
        unsafe {
            let mut b = [0x55u8; 40];
            let r = _itoa(255, b.as_mut_ptr() as *mut c_char, 16);
            assert_eq!(r as *const u8, b.as_ptr());
            assert_eq!(st(&b), "ff");
            _itoa(-123, b.as_mut_ptr() as *mut c_char, 10);
            assert_eq!(st(&b), "-123");
            // negative values are unsigned in other radixes
            _itoa(-1, b.as_mut_ptr() as *mut c_char, 16);
            assert_eq!(st(&b), "ffffffff");
            _itoa(5, b.as_mut_ptr() as *mut c_char, 2);
            assert_eq!(st(&b), "101");
            _itoa(0, b.as_mut_ptr() as *mut c_char, 36);
            assert_eq!(st(&b), "0");
            _itoa(i32::MIN, b.as_mut_ptr() as *mut c_char, 10);
            assert_eq!(st(&b), "-2147483648");
            _itoa(35, b.as_mut_ptr() as *mut c_char, 36);
            assert_eq!(st(&b), "z");
            _itoa(7, b.as_mut_ptr() as *mut c_char, 1);
            assert_eq!(st(&b), "");
            assert!(_itoa(7, std::ptr::null_mut(), 10).is_null());

            let mut d = [0x55u16; 12];
            assert_eq!(_itow_s(-2147483648, d.as_mut_ptr(), 12, 10), 0);
            assert_eq!(ws(&d), "-2147483648");
            assert_eq!(_itow_s(255, d.as_mut_ptr(), 12, 16), 0);
            assert_eq!(ws(&d), "ff");
            let mut big = [0x55u16; 33];
            assert_eq!(_itow_s(-1, big.as_mut_ptr(), 33, 2), 0);
            assert_eq!(ws(&big), "1".repeat(32));
        }
    }

    #[test]
    fn itow_s_errors() {
        unsafe {
            let mut d = [0x55u16; 4];
            assert_eq!(_itow_s(123, d.as_mut_ptr(), 4, 10), 0);
            assert_eq!(ws(&d), "123");
            assert_eq!(_itow_s(1234, d.as_mut_ptr(), 4, 10), ERANGE);
            assert_eq!(d[0], 0);
            let mut d = [0x55u16; 4];
            assert_eq!(_itow_s(-12, d.as_mut_ptr(), 3, 10), ERANGE);
            assert_eq!(d[0], 0);
            let mut d = [0x55u16; 4];
            assert_eq!(_itow_s(5, d.as_mut_ptr(), 4, 1), EINVAL);
            assert_eq!(d[0], 0);
            let mut d = [0x55u16; 4];
            assert_eq!(_itow_s(5, d.as_mut_ptr(), 4, 37), EINVAL);
            assert_eq!(_itow_s(5, std::ptr::null_mut(), 4, 10), EINVAL);
            let mut d = [0x55u16; 4];
            assert_eq!(_itow_s(5, d.as_mut_ptr(), 0, 10), EINVAL);
            assert_eq!(d[0], 0x55);
        }
    }

    #[test]
    fn btowc_cases() {
        unsafe {
            assert_eq!(btowc('A' as c_int), 65);
            assert_eq!(btowc(0), 0);
            assert_eq!(btowc(0xE9), 0xE9);
            assert_eq!(btowc(-1), WEOF);
            assert_eq!(btowc(256), WEOF);
        }
    }

    #[test]
    fn wcstombs_cases() {
        unsafe {
            let src = w("abc\u{e9}");
            let mut d = [0x55u8; 8];
            assert_eq!(wcstombs(d.as_mut_ptr() as *mut c_char, src.as_ptr(), 8), 4);
            assert_eq!(&d[..5], &[b'a', b'b', b'c', 0xE9, 0]);
            assert_eq!(wcstombs(std::ptr::null_mut(), src.as_ptr(), 0), 4);
            let mut d = [0x55u8; 8];
            assert_eq!(wcstombs(d.as_mut_ptr() as *mut c_char, src.as_ptr(), 2), 2);
            assert_eq!(&d[..3], &[b'a', b'b', 0x55]);
            assert_eq!(
                wcstombs(d.as_mut_ptr() as *mut c_char, w("a€").as_ptr(), 8),
                usize::MAX
            );
            assert_eq!(
                wcstombs(d.as_mut_ptr() as *mut c_char, std::ptr::null(), 8),
                usize::MAX
            );
        }
    }

    #[test]
    fn wcstombs_s_cases() {
        unsafe {
            let mut ret = 99usize;
            let mut d = [0x55u8; 8];
            let dp = d.as_mut_ptr() as *mut c_char;
            assert_eq!(wcstombs_s(&mut ret, dp, 8, w("abc").as_ptr(), TRUNCATE), 0);
            assert_eq!(ret, 4);
            assert_eq!(st(&d), "abc");
            // size query
            assert_eq!(
                wcstombs_s(
                    &mut ret,
                    std::ptr::null_mut(),
                    0,
                    w("hello").as_ptr(),
                    TRUNCATE
                ),
                0
            );
            assert_eq!(ret, 6);
            // count limits the copy
            assert_eq!(wcstombs_s(&mut ret, dp, 8, w("abcdef").as_ptr(), 2), 0);
            assert_eq!((ret, st(&d).as_str()), (3, "ab"));
            // too small
            assert_eq!(wcstombs_s(&mut ret, dp, 4, w("abcd").as_ptr(), 4), ERANGE);
            assert_eq!((ret, d[0]), (0, 0));
            assert_eq!(
                wcstombs_s(&mut ret, dp, 4, w("abcd").as_ptr(), TRUNCATE),
                STRUNCATE
            );
            assert_eq!((ret, st(&d).as_str()), (4, "abc"));
            // exact fit
            assert_eq!(wcstombs_s(&mut ret, dp, 5, w("abcd").as_ptr(), TRUNCATE), 0);
            assert_eq!(ret, 5);
            // unconvertible
            assert_eq!(
                wcstombs_s(&mut ret, dp, 8, w("a€").as_ptr(), TRUNCATE),
                EILSEQ
            );
            assert_eq!((ret, d[0]), (0, 0));
            // a size query only counts: unconvertible characters are not reported
            ret = 99;
            let q = wcstombs_s(&mut ret, std::ptr::null_mut(), 0, w("\u{fffd}").as_ptr(), 1);
            assert_eq!((q, ret), (0, 2));
            // bad arguments
            assert_eq!(
                wcstombs_s(&mut ret, dp, 8, std::ptr::null(), TRUNCATE),
                EINVAL
            );
            assert_eq!(d[0], 0);
            assert_eq!(
                wcstombs_s(&mut ret, std::ptr::null_mut(), 4, w("a").as_ptr(), 1),
                EINVAL
            );
            assert_eq!(wcstombs_s(&mut ret, dp, 0, w("a").as_ptr(), 1), EINVAL);
            // null return pointer is allowed
            assert_eq!(
                wcstombs_s(std::ptr::null_mut(), dp, 8, w("x").as_ptr(), TRUNCATE),
                0
            );
        }
    }

    #[test]
    fn errno_on_conversion_errors() {
        use crate::api_ms_win_crt_runtime::{errno, set_errno, test_support::*};
        let _g = install_counting_handler();
        unsafe {
            set_errno(0xdead);
            assert_eq!(
                strtol(c"99999999999".as_ptr(), std::ptr::null_mut(), 10),
                i32::MAX
            );
            assert_eq!(errno(), ERANGE);
            set_errno(0xdead);
            assert_eq!(strtoul(c"42".as_ptr(), std::ptr::null_mut(), 10), 42);
            assert_eq!(errno(), 0xdead);
            assert_eq!(
                strtoull(c"18446744073709551616".as_ptr(), std::ptr::null_mut(), 10),
                u64::MAX
            );
            assert_eq!(errno(), ERANGE);
            // Bad base: EINVAL and handler.
            set_errno(0);
            assert_eq!(strtol(c"1".as_ptr(), std::ptr::null_mut(), 1), 0);
            assert_eq!(errno(), EINVAL);
            assert_eq!(calls(), 1);
            // strtod: overflow and underflow to zero, but not a denormal result.
            set_errno(0xdead);
            assert_eq!(
                strtod(c"1e999".as_ptr(), std::ptr::null_mut()),
                f64::INFINITY
            );
            assert_eq!(errno(), ERANGE);
            set_errno(0xdead);
            assert_eq!(strtod(c"2.47e-324".as_ptr(), std::ptr::null_mut()), 0.0);
            assert_eq!(errno(), ERANGE);
            set_errno(0xdead);
            assert_eq!(strtod(c"inf".as_ptr(), std::ptr::null_mut()), f64::INFINITY);
            assert_eq!(strtod(c"2.48e-324".as_ptr(), std::ptr::null_mut()), 5e-324);
            assert_eq!(errno(), 0xdead);
            set_errno(0xdead);
            assert_eq!(
                strtof(c"1.e40".as_ptr(), std::ptr::null_mut()),
                f32::INFINITY
            );
            assert_eq!(errno(), ERANGE);
            // wcstombs_s: EILSEQ sets errno without calling the handler.
            let mut d = [0 as c_char; 4];
            let mut n = 0usize;
            set_errno(0);
            let src = [0x20ACu16, 0];
            assert_eq!(
                wcstombs_s(&mut n, d.as_mut_ptr(), 4, src.as_ptr(), 4),
                EILSEQ
            );
            assert_eq!(errno(), EILSEQ);
            set_errno(0);
            assert_eq!(wcstombs(d.as_mut_ptr(), src.as_ptr(), 4), usize::MAX);
            assert_eq!(errno(), EILSEQ);
            assert_eq!(calls(), 1);
            // _itow_s range error.
            let mut w = [0u16; 2];
            set_errno(0);
            assert_eq!(_itow_s(12345, w.as_mut_ptr(), 2, 10), ERANGE);
            assert_eq!(errno(), ERANGE);
            assert_eq!(calls(), 2);
        }
    }
}
