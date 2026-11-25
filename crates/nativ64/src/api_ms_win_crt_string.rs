//! `api-ms-win-crt-string`: the UCRT string and character functions.
//!
//! ABI notes (MSVC x64): `wchar_t`/`wint_t` are 16 bits (UTF-16 code units), `size_t` is 64
//! bits, `errno_t` is `int`. The `wcs*`/`isw*` functions therefore cannot forward to glibc.
//!
//! Locale: only the default "C" locale exists. `is*`/`to*` are ASCII-only and `_wcsicmp`-style
//! folding is ASCII-only as in the UCRT's "C" locale; `isw*` use Unicode properties for
//! non-ASCII code units (the UCRT's wide ctype table is locale independent) except
//! `iswdigit`/`iswxdigit`, which are ASCII-only. Comparison functions return -1, 0 or 1.
//!
//! The secure (`_s`) functions return `errno_t` (`EINVAL`, `ERANGE`, `STRUNCATE`) and clear
//! `dst[0]` where the UCRT does. TODO: they should also call the invalid parameter handler;
//! that is a no-op for now. There is no CRT `errno` yet, so it is never set. The UCRT's debug
//! `0xFE` fill of the rest of the destination is not reproduced.
//!
//! `_wcsdup` allocates with libc `malloc` (to be freed with the CRT `free`).
//! `strtok` keeps its state per thread; `strtok_s`/`wcstok_s` use the caller's context.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use std::cell::Cell;
use std::ffi::{c_char, c_int};

const EINVAL: i32 = 22;
const ERANGE: i32 = 34;
const STRUNCATE: i32 = 80;
/// `_TRUNCATE`: `(size_t)-1`.
const TRUNCATE: usize = usize::MAX;

/// A character unit: `u8` (narrow) or `u16` (wide).
trait Unit: Copy + PartialEq + Default + Into<u32> {}
impl Unit for u8 {}
impl Unit for u16 {}

fn is_nul<T: Unit>(c: T) -> bool {
    c == T::default()
}

unsafe fn len<T: Unit>(p: *const T) -> usize {
    let mut n = 0;
    while !is_nul(*p.add(n)) {
        n += 1;
    }
    n
}

/// Length of `p`, looking at no more than `max` units.
unsafe fn nlen<T: Unit>(p: *const T, max: usize) -> usize {
    let mut n = 0;
    while n < max && !is_nul(*p.add(n)) {
        n += 1;
    }
    n
}

fn sign(a: u32, b: u32) -> c_int {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// ASCII-only case fold (the "C" locale).
fn fold(c: u32) -> u32 {
    if (b'A' as u32..=b'Z' as u32).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// Compares at most `max` units; `icase` folds ASCII letters.
unsafe fn cmp<T: Unit>(a: *const T, b: *const T, max: usize, icase: bool) -> c_int {
    for i in 0..max {
        let (mut x, mut y): (u32, u32) = ((*a.add(i)).into(), (*b.add(i)).into());
        if icase {
            x = fold(x);
            y = fold(y);
        }
        if x != y {
            return sign(x, y);
        }
        if x == 0 {
            break;
        }
    }
    0
}

unsafe fn in_set<T: Unit>(c: T, set: *const T) -> bool {
    let mut i = 0;
    while !is_nul(*set.add(i)) {
        if *set.add(i) == c {
            return true;
        }
        i += 1;
    }
    false
}

unsafe fn span<T: Unit>(s: *const T, set: *const T, want_in: bool) -> usize {
    let mut n = 0;
    while !is_nul(*s.add(n)) && in_set(*s.add(n), set) == want_in {
        n += 1;
    }
    n
}

unsafe fn pbrk<T: Unit>(s: *const T, set: *const T) -> *mut T {
    let n = span(s, set, false);
    if is_nul(*s.add(n)) {
        std::ptr::null_mut()
    } else {
        s.add(n) as *mut T
    }
}

unsafe fn copy_s<T: Unit>(dst: *mut T, size: usize, src: *const T) -> i32 {
    if dst.is_null() || size == 0 {
        return EINVAL;
    }
    if src.is_null() {
        *dst = T::default();
        return EINVAL;
    }
    let n = len(src);
    if n + 1 > size {
        *dst = T::default();
        return ERANGE;
    }
    std::ptr::copy(src, dst, n + 1);
    0
}

unsafe fn cat_s<T: Unit>(dst: *mut T, size: usize, src: *const T) -> i32 {
    if dst.is_null() || size == 0 {
        return EINVAL;
    }
    if src.is_null() {
        *dst = T::default();
        return EINVAL;
    }
    let d = nlen(dst, size);
    if d == size {
        *dst = T::default();
        return EINVAL;
    }
    let n = len(src);
    if n + 1 > size - d {
        *dst = T::default();
        return ERANGE;
    }
    std::ptr::copy(src, dst.add(d), n + 1);
    0
}

unsafe fn ncpy_s<T: Unit>(dst: *mut T, size: usize, src: *const T, count: usize) -> i32 {
    if dst.is_null() || size == 0 {
        return EINVAL;
    }
    if count == 0 {
        *dst = T::default();
        return 0;
    }
    if src.is_null() {
        *dst = T::default();
        return EINVAL;
    }
    if count == TRUNCATE {
        let n = len(src);
        let c = n.min(size - 1);
        std::ptr::copy(src, dst, c);
        *dst.add(c) = T::default();
        return if n > size - 1 { STRUNCATE } else { 0 };
    }
    let n = nlen(src, count);
    if n + 1 > size {
        *dst = T::default();
        return ERANGE;
    }
    std::ptr::copy(src, dst, n);
    *dst.add(n) = T::default();
    0
}

unsafe fn ncat_s<T: Unit>(dst: *mut T, size: usize, src: *const T, count: usize) -> i32 {
    if dst.is_null() || size == 0 {
        return EINVAL;
    }
    let d = nlen(dst, size);
    if d == size {
        *dst = T::default();
        return EINVAL;
    }
    if count == 0 {
        return 0;
    }
    if src.is_null() {
        *dst = T::default();
        return EINVAL;
    }
    let avail = size - d;
    let n = if count == TRUNCATE {
        len(src)
    } else {
        nlen(src, count)
    };
    if n + 1 > avail {
        if count == TRUNCATE {
            std::ptr::copy(src, dst.add(d), avail - 1);
            *dst.add(size - 1) = T::default();
            return STRUNCATE;
        }
        *dst = T::default();
        return ERANGE;
    }
    std::ptr::copy(src, dst.add(d), n);
    *dst.add(d + n) = T::default();
    0
}

/// `strncpy`: copies at most `n` units and pads with NULs up to `n`.
unsafe fn ncpy<T: Unit>(dst: *mut T, src: *const T, n: usize) -> *mut T {
    let l = nlen(src, n);
    std::ptr::copy(src, dst, l);
    for i in l..n {
        *dst.add(i) = T::default();
    }
    dst
}

unsafe fn ncat<T: Unit>(dst: *mut T, src: *const T, n: usize) -> *mut T {
    let d = len(dst);
    let l = nlen(src, n);
    std::ptr::copy(src, dst.add(d), l);
    *dst.add(d + l) = T::default();
    dst
}

/// Shared tokenizer: `ctx` holds where the next call resumes.
unsafe fn tok<T: Unit>(s: *mut T, delim: *const T, ctx: &mut *mut T) -> *mut T {
    let mut p = if s.is_null() { *ctx } else { s };
    if p.is_null() {
        return std::ptr::null_mut();
    }
    while !is_nul(*p) && in_set(*p, delim) {
        p = p.add(1);
    }
    if is_nul(*p) {
        *ctx = p;
        return std::ptr::null_mut();
    }
    let start = p;
    while !is_nul(*p) && !in_set(*p, delim) {
        p = p.add(1);
    }
    if is_nul(*p) {
        *ctx = p;
    } else {
        *p = T::default();
        *ctx = p.add(1);
    }
    start
}

unsafe fn tok_s<T: Unit>(s: *mut T, delim: *const T, ctx: *mut *mut T) -> *mut T {
    if ctx.is_null() || delim.is_null() || (s.is_null() && (*ctx).is_null()) {
        return std::ptr::null_mut();
    }
    tok(s, delim, &mut *ctx)
}

thread_local! {
    static STRTOK_NEXT: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
}

// C-locale classification of an `int` (valid for 0..=255, 0 elsewhere).
fn ascii(c: c_int) -> Option<u8> {
    u8::try_from(c).ok().filter(u8::is_ascii)
}
fn wide(c: u16) -> Option<char> {
    char::from_u32(c as u32)
}

fn iswspace_(c: u16) -> bool {
    c == 0x85 || (c != 0xA0 && c != 0x2007 && wide(c).is_some_and(char::is_whitespace))
}
fn iswalpha_(c: u16) -> bool {
    wide(c).is_some_and(char::is_alphabetic)
}

/// Exposed for the convert module (`atoi`-style whitespace skipping).
pub(crate) fn is_space_unit(c: u32) -> bool {
    matches!(c, 9..=13 | 32)
}

win32! {
    module "api-ms-win-crt-string" => "ucrtbase";
    Native fn __strncnt(s: *const c_char, max: usize) -> usize { nlen(s as *const u8, max) }
    Native fn _stricmp(a: *const c_char, b: *const c_char) -> c_int {
        cmp(a as *const u8, b as *const u8, usize::MAX, true)
    }
    Native fn _strnicmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
        cmp(a as *const u8, b as *const u8, n, true)
    }
    Native fn _wcsdup(s: *const u16) -> *mut u16 {
        if s.is_null() {
            return std::ptr::null_mut();
        }
        let n = len(s) + 1;
        let p = libc::malloc(n * 2) as *mut u16;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(s, p, n);
        }
        p
    }
    Native fn _wcsicoll(a: *const u16, b: *const u16) -> c_int { cmp(a, b, usize::MAX, true) }
    Native fn _wcsnicmp(a: *const u16, b: *const u16, n: usize) -> c_int { cmp(a, b, n, true) }
    Native fn isalnum(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_alphanumeric()) as c_int }
    Native fn isalpha(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_alphabetic()) as c_int }
    Native fn isdigit(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_digit()) as c_int }
    Native fn islower(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_lowercase()) as c_int }
    Native fn isspace(c: c_int) -> c_int { ascii(c).is_some_and(|b| is_space_unit(b as u32)) as c_int }
    Native fn isupper(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_uppercase()) as c_int }
    Native fn isxdigit(c: c_int) -> c_int { ascii(c).is_some_and(|b| b.is_ascii_hexdigit()) as c_int }
    Native fn iswalnum(c: u16) -> c_int { (iswalpha_(c) || c.wrapping_sub(0x30) < 10) as c_int }
    Native fn iswdigit(c: u16) -> c_int { (c.wrapping_sub(0x30) < 10) as c_int }
    Native fn iswspace(c: u16) -> c_int { iswspace_(c) as c_int }
    Native fn iswxdigit(c: u16) -> c_int {
        (u8::try_from(c).is_ok_and(|b| b.is_ascii_hexdigit())) as c_int
    }
    Native fn strcat(dst: *mut c_char, src: *const c_char) -> *mut c_char {
        ncat(dst as *mut u8, src as *const u8, usize::MAX);
        dst
    }
    Native fn strcat_s(dst: *mut c_char, size: usize, src: *const c_char) -> i32 {
        cat_s(dst as *mut u8, size, src as *const u8)
    }
    Native fn strcmp(a: *const c_char, b: *const c_char) -> c_int {
        cmp(a as *const u8, b as *const u8, usize::MAX, false)
    }
    Native fn strcpy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
        std::ptr::copy(src as *const u8, dst as *mut u8, len(src as *const u8) + 1);
        dst
    }
    Native fn strcpy_s(dst: *mut c_char, size: usize, src: *const c_char) -> i32 {
        copy_s(dst as *mut u8, size, src as *const u8)
    }
    Native fn strcspn(s: *const c_char, set: *const c_char) -> usize {
        span(s as *const u8, set as *const u8, false)
    }
    Native fn strlen(s: *const c_char) -> usize { len(s as *const u8) }
    Native fn strncat(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
        ncat(dst as *mut u8, src as *const u8, n);
        dst
    }
    Native fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
        cmp(a as *const u8, b as *const u8, n, false)
    }
    Native fn strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
        ncpy(dst as *mut u8, src as *const u8, n);
        dst
    }
    Native fn strncpy_s(dst: *mut c_char, size: usize, src: *const c_char, count: usize) -> i32 {
        ncpy_s(dst as *mut u8, size, src as *const u8, count)
    }
    Native fn strpbrk(s: *const c_char, set: *const c_char) -> *mut c_char {
        pbrk(s as *const u8, set as *const u8) as *mut c_char
    }
    Native fn strspn(s: *const c_char, set: *const c_char) -> usize {
        span(s as *const u8, set as *const u8, true)
    }
    Native fn strtok(s: *mut c_char, delim: *const c_char) -> *mut c_char {
        if delim.is_null() {
            return std::ptr::null_mut();
        }
        STRTOK_NEXT.with(|n| {
            let mut ctx = n.get();
            let r = tok(s as *mut u8, delim as *const u8, &mut ctx);
            n.set(ctx);
            r as *mut c_char
        })
    }
    Native fn strtok_s(s: *mut c_char, delim: *const c_char, ctx: *mut *mut c_char) -> *mut c_char {
        tok_s(s as *mut u8, delim as *const u8, ctx as *mut *mut u8) as *mut c_char
    }
    Native fn tolower(c: c_int) -> c_int {
        if (65..=90).contains(&c) { c + 32 } else { c }
    }
    Native fn toupper(c: c_int) -> c_int {
        if (97..=122).contains(&c) { c - 32 } else { c }
    }
    Native fn wcscat_s(dst: *mut u16, size: usize, src: *const u16) -> i32 { cat_s(dst, size, src) }
    Native fn wcscmp(a: *const u16, b: *const u16) -> c_int { cmp(a, b, usize::MAX, false) }
    // "C" locale: collation is code-unit order.
    Native fn wcscoll(a: *const u16, b: *const u16) -> c_int { cmp(a, b, usize::MAX, false) }
    Native fn wcscpy_s(dst: *mut u16, size: usize, src: *const u16) -> i32 { copy_s(dst, size, src) }
    Native fn wcslen(s: *const u16) -> usize { len(s) }
    Native fn wcsncat_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
        ncat_s(dst, size, src, count)
    }
    Native fn wcsncmp(a: *const u16, b: *const u16, n: usize) -> c_int { cmp(a, b, n, false) }
    Native fn wcsncpy(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 { ncpy(dst, src, n) }
    Native fn wcsncpy_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
        ncpy_s(dst, size, src, count)
    }
    // Unlike `wcslen`, a NULL pointer yields 0.
    Native fn wcsnlen(s: *const u16, max: usize) -> usize {
        if s.is_null() { 0 } else { nlen(s, max) }
    }
    Native fn wcstok_s(s: *mut u16, delim: *const u16, ctx: *mut *mut u16) -> *mut u16 {
        tok_s(s, delim, ctx)
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
    fn s(p: *const c_char) -> String {
        unsafe { CStr::from_ptr(p).to_str().unwrap().to_string() }
    }
    fn ws(v: &[u16]) -> String {
        let n = v.iter().position(|&x| x == 0).unwrap();
        String::from_utf16(&v[..n]).unwrap()
    }
    fn mp(v: &mut [u8]) -> *mut c_char {
        v.as_mut_ptr() as *mut c_char
    }

    #[test]
    fn registry_calls() {
        let get = |n: &str| {
            crate::lookup("api-ms-win-crt-string-l1-1-0.dll", n)
                .unwrap()
                .addr
        };
        unsafe {
            let f: unsafe extern "win64" fn(*const c_char) -> usize =
                std::mem::transmute(get("strlen"));
            assert_eq!(f(p(&c("hello"))), 5);
            let f: unsafe extern "win64" fn(*mut u16, usize, *const u16, usize) -> i32 =
                std::mem::transmute(get("wcsncpy_s"));
            let mut d = [1u16; 4];
            assert_eq!(
                f(d.as_mut_ptr(), 4, w("abcdef").as_ptr(), TRUNCATE),
                STRUNCATE
            );
            assert_eq!(ws(&d), "abc");
            let f: unsafe extern "win64" fn(c_int) -> c_int = std::mem::transmute(get("toupper"));
            assert_eq!(f('q' as c_int), 'Q' as c_int);
        }
        assert!(crate::lookup("ucrtbase", "wcstok_s").is_some());
    }

    #[test]
    fn every_name_registered() {
        let names = "__strncnt _stricmp _strnicmp _wcsdup _wcsicoll _wcsnicmp isalnum isalpha isdigit islower isspace isupper iswalnum iswdigit iswspace iswxdigit isxdigit strcat strcat_s strcmp strcpy strcpy_s strcspn strlen strncat strncmp strncpy strncpy_s strpbrk strspn strtok strtok_s tolower toupper wcscat_s wcscmp wcscoll wcscpy_s wcslen wcsncat_s wcsncmp wcsncpy wcsncpy_s wcsnlen wcstok_s";
        assert_eq!(names.split(' ').count(), 45);
        for n in names.split(' ') {
            assert!(crate::lookup("api-ms-win-crt-string", n).is_some(), "{n}");
        }
    }

    #[test]
    fn lengths_and_compare() {
        unsafe {
            assert_eq!(strlen(p(&c(""))), 0);
            assert_eq!(strlen(p(&c("abc"))), 3);
            assert_eq!(__strncnt(p(&c("abcdef")), 4), 4);
            assert_eq!(__strncnt(p(&c("ab")), 10), 2);
            assert_eq!(wcslen(w("héllo€").as_ptr()), 6);
            assert_eq!(wcsnlen(w("abc").as_ptr(), 2), 2);
            assert_eq!(wcsnlen(std::ptr::null(), 5), 0);
            assert_eq!(strcmp(p(&c("a")), p(&c("a"))), 0);
            assert_eq!(strcmp(p(&c("a")), p(&c("b"))), -1);
            assert_eq!(strcmp(p(&c("b")), p(&c("a"))), 1);
            assert_eq!(strcmp(p(&c("ab")), p(&c("a"))), 1);
            // bytes compare unsigned
            assert_eq!(strcmp(p(&[0xE9, 0]), p(&c("a"))), 1);
            assert_eq!(strncmp(p(&c("abcd")), p(&c("abxx")), 2), 0);
            assert_eq!(strncmp(p(&c("abcd")), p(&c("abxx")), 3), -1);
            assert_eq!(strncmp(p(&c("a")), p(&c("b")), 0), 0);
            assert_eq!(_stricmp(p(&c("HeLLo")), p(&c("hello"))), 0);
            assert_eq!(_stricmp(p(&c("a")), p(&c("B"))), -1);
            assert_eq!(_strnicmp(p(&c("ABCx")), p(&c("abcy")), 3), 0);
            assert_eq!(_strnicmp(p(&c("ABCx")), p(&c("abcy")), 4), -1);
            assert_eq!(wcscmp(w("é").as_ptr(), w("e").as_ptr()), 1);
            assert_eq!(wcscmp(w("\u{ff21}").as_ptr(), w("\u{ff41}").as_ptr()), -1);
            assert_eq!(wcscoll(w("a").as_ptr(), w("b").as_ptr()), -1);
            assert_eq!(wcsncmp(w("abcd").as_ptr(), w("abce").as_ptr(), 3), 0);
            assert_eq!(_wcsicoll(w("ABC").as_ptr(), w("abc").as_ptr()), 0);
            assert_eq!(_wcsnicmp(w("ABz").as_ptr(), w("abc").as_ptr(), 2), 0);
            assert_eq!(_wcsnicmp(w("ABz").as_ptr(), w("abc").as_ptr(), 3), 1);
            // C locale: no folding of non-ASCII
            assert_eq!(_wcsnicmp(w("É").as_ptr(), w("é").as_ptr(), 1), -1);
        }
    }

    #[test]
    fn classification() {
        unsafe {
            assert_eq!(isalnum('a' as c_int), 1);
            assert_eq!(isalnum('_' as c_int), 0);
            assert_eq!(isalpha('Z' as c_int), 1);
            assert_eq!(isalpha('1' as c_int), 0);
            assert_eq!(isdigit('7' as c_int), 1);
            assert_eq!(isdigit('a' as c_int), 0);
            assert_eq!(islower('a' as c_int), 1);
            assert_eq!(islower('A' as c_int), 0);
            assert_eq!(isupper('A' as c_int), 1);
            assert_eq!(isspace(' ' as c_int), 1);
            assert_eq!(isspace('\n' as c_int), 1);
            assert_eq!(isspace('x' as c_int), 0);
            assert_eq!(isxdigit('F' as c_int), 1);
            assert_eq!(isxdigit('g' as c_int), 0);
            // EOF, out of range and high bytes are not classified in the C locale
            for f in [
                isalnum, isalpha, isdigit, islower, isspace, isupper, isxdigit,
            ] {
                assert_eq!(f(-1), 0);
                assert_eq!(f(0xE9), 0);
                assert_eq!(f(1000), 0);
            }
            assert_eq!(tolower('Q' as c_int), 'q' as c_int);
            assert_eq!(tolower('q' as c_int), 'q' as c_int);
            assert_eq!(tolower(-1), -1);
            assert_eq!(tolower(0xC9), 0xC9);
            assert_eq!(toupper('q' as c_int), 'Q' as c_int);
            assert_eq!(toupper('1' as c_int), '1' as c_int);
            assert_eq!(iswalnum('a' as u16), 1);
            assert_eq!(iswalnum(0xE9), 1);
            assert_eq!(iswalnum('3' as u16), 1);
            assert_eq!(iswalnum('-' as u16), 0);
            assert_eq!(iswdigit('9' as u16), 1);
            assert_eq!(iswdigit(0x0663), 0);
            assert_eq!(iswspace(' ' as u16), 1);
            assert_eq!(iswspace(0x3000), 1);
            assert_eq!(iswspace('a' as u16), 0);
            assert_eq!(iswxdigit('e' as u16), 1);
            assert_eq!(iswxdigit('g' as u16), 0);
            assert_eq!(iswxdigit(0x0165), 0);
        }
    }

    #[test]
    fn copy_and_cat() {
        unsafe {
            let mut d = [0xAAu8; 16];
            strcpy(mp(&mut d), p(&c("foo")));
            assert_eq!(strcat(mp(&mut d), p(&c("bar"))), mp(&mut d));
            assert_eq!(s(p(&d)), "foobar");
            strncat(mp(&mut d), p(&c("bazqux")), 3);
            assert_eq!(s(p(&d)), "foobarbaz");
            let mut d = [0xAAu8; 6];
            strncpy(mp(&mut d), p(&c("ab")), 5);
            assert_eq!(d, [b'a', b'b', 0, 0, 0, 0xAA]);
            let mut d = [0xAAu16; 5];
            wcsncpy(d.as_mut_ptr(), w("é€").as_ptr(), 4);
            assert_eq!(d, [0xE9, 0x20AC, 0, 0, 0xAA]);
            let mut d = [0xAAu8; 3];
            strncpy(mp(&mut d), p(&c("abcdef")), 3);
            assert_eq!(d, *b"abc");
        }
    }

    #[test]
    fn copy_s_errors() {
        unsafe {
            let mut d = [0x55u8; 4];
            assert_eq!(strcpy_s(mp(&mut d), 4, p(&c("abc"))), 0);
            assert_eq!(s(p(&d)), "abc");
            assert_eq!(strcpy_s(mp(&mut d), 4, p(&c("abcd"))), ERANGE);
            assert_eq!(d[0], 0);
            let mut d = [0x55u8; 4];
            assert_eq!(strcpy_s(mp(&mut d), 4, std::ptr::null()), EINVAL);
            assert_eq!(d[0], 0);
            assert_eq!(strcpy_s(std::ptr::null_mut(), 4, p(&c("a"))), EINVAL);
            let mut d = [0x55u8; 4];
            assert_eq!(strcpy_s(mp(&mut d), 0, p(&c("a"))), EINVAL);
            assert_eq!(d[0], 0x55);
            let mut d = [0x55u16; 3];
            assert_eq!(wcscpy_s(d.as_mut_ptr(), 3, w("é€").as_ptr()), 0);
            assert_eq!(ws(&d), "é€");
            assert_eq!(wcscpy_s(d.as_mut_ptr(), 3, w("abc").as_ptr()), ERANGE);
            assert_eq!(d[0], 0);
            assert_eq!(wcscpy_s(d.as_mut_ptr(), 3, std::ptr::null()), EINVAL);
            assert_eq!(wcscpy_s(std::ptr::null_mut(), 3, w("a").as_ptr()), EINVAL);
        }
    }

    #[test]
    fn cat_s_errors() {
        unsafe {
            let mut d = [0u8; 8];
            d[..2].copy_from_slice(b"ab");
            assert_eq!(strcat_s(mp(&mut d), 8, p(&c("cd"))), 0);
            assert_eq!(s(p(&d)), "abcd");
            assert_eq!(strcat_s(mp(&mut d), 8, p(&c("efgh"))), ERANGE);
            assert_eq!(d[0], 0);
            let mut d = *b"ab\0xxxxx";
            assert_eq!(strcat_s(mp(&mut d), 8, std::ptr::null()), EINVAL);
            assert_eq!(d[0], 0);
            let mut d = *b"abcd";
            // destination not terminated within `size`
            assert_eq!(strcat_s(mp(&mut d), 4, p(&c("x"))), EINVAL);
            assert_eq!(d[0], 0);
            assert_eq!(strcat_s(std::ptr::null_mut(), 4, p(&c("x"))), EINVAL);
            let mut d = [0u16; 6];
            d[0] = 'a' as u16;
            assert_eq!(wcscat_s(d.as_mut_ptr(), 6, w("é€").as_ptr()), 0);
            assert_eq!(ws(&d), "aé€");
            assert_eq!(wcscat_s(d.as_mut_ptr(), 6, w("xyz").as_ptr()), ERANGE);
            assert_eq!(d[0], 0);
            assert_eq!(wcscat_s(d.as_mut_ptr(), 0, w("x").as_ptr()), EINVAL);
        }
    }

    #[test]
    fn ncpy_s_cases() {
        unsafe {
            let mut d = [0x55u8; 5];
            assert_eq!(strncpy_s(mp(&mut d), 5, p(&c("abcdef")), 3), 0);
            assert_eq!(s(p(&d)), "abc");
            assert_eq!(strncpy_s(mp(&mut d), 5, p(&c("ab")), 10), 0);
            assert_eq!(s(p(&d)), "ab");
            assert_eq!(strncpy_s(mp(&mut d), 5, p(&c("abcdef")), 5), ERANGE);
            assert_eq!(d[0], 0);
            assert_eq!(
                strncpy_s(mp(&mut d), 5, p(&c("abcdef")), TRUNCATE),
                STRUNCATE
            );
            assert_eq!(s(p(&d)), "abcd");
            assert_eq!(strncpy_s(mp(&mut d), 5, p(&c("abcd")), TRUNCATE), 0);
            assert_eq!(s(p(&d)), "abcd");
            assert_eq!(strncpy_s(mp(&mut d), 5, p(&c("zz")), 0), 0);
            assert_eq!(d[0], 0);
            assert_eq!(strncpy_s(mp(&mut d), 5, std::ptr::null(), 2), EINVAL);
            assert_eq!(strncpy_s(std::ptr::null_mut(), 5, p(&c("a")), 1), EINVAL);
            assert_eq!(strncpy_s(mp(&mut d), 0, p(&c("a")), 1), EINVAL);
            let mut d = [0x55u16; 4];
            assert_eq!(wcsncpy_s(d.as_mut_ptr(), 4, w("é€x€").as_ptr(), 2), 0);
            assert_eq!(ws(&d), "é€");
            assert_eq!(
                wcsncpy_s(d.as_mut_ptr(), 4, w("é€x€").as_ptr(), TRUNCATE),
                STRUNCATE
            );
            assert_eq!(ws(&d), "é€x");
            assert_eq!(wcsncpy_s(d.as_mut_ptr(), 4, w("é€x€").as_ptr(), 4), ERANGE);
            assert_eq!(d[0], 0);
            assert_eq!(wcsncpy_s(d.as_mut_ptr(), 4, std::ptr::null(), 1), EINVAL);
        }
    }

    #[test]
    fn ncat_s_cases() {
        unsafe {
            let mut d = [0u8; 8];
            d[..2].copy_from_slice(b"ab");
            assert_eq!(strncat_s_(&mut d, "cdef", 2), 0);
            assert_eq!(s(p(&d)), "abcd");
            assert_eq!(strncat_s_(&mut d, "xy", 10), 0);
            assert_eq!(s(p(&d)), "abcdxy");
            assert_eq!(strncat_s_(&mut d, "123456", TRUNCATE), STRUNCATE);
            assert_eq!(s(p(&d)), "abcdxy1");
            let mut d = *b"ab\0\0";
            assert_eq!(strncat_s_(&mut d, "cdef", 3), ERANGE);
            assert_eq!(d[0], 0);
            let mut d = *b"ab\0\0";
            assert_eq!(strncat_s_(&mut d, "zzz", 0), 0);
            assert_eq!(s(p(&d)), "ab");
            let mut d = *b"ab\0\0";
            assert_eq!(wcs_null_src_(&mut d), EINVAL);
            let mut d = [0u16; 5];
            d[0] = 'a' as u16;
            assert_eq!(wcsncat_s(d.as_mut_ptr(), 5, w("é€xyz").as_ptr(), 2), 0);
            assert_eq!(ws(&d), "aé€");
            assert_eq!(
                wcsncat_s(d.as_mut_ptr(), 5, w("xyz").as_ptr(), TRUNCATE),
                STRUNCATE
            );
            assert_eq!(ws(&d), "aé€x");
            assert_eq!(wcsncat_s(d.as_mut_ptr(), 5, w("y").as_ptr(), 1), ERANGE);
            assert_eq!(d[0], 0);
            assert_eq!(
                wcsncat_s(std::ptr::null_mut(), 5, w("y").as_ptr(), 1),
                EINVAL
            );
        }
    }

    unsafe fn strncat_s_(d: &mut [u8], src: &str, n: usize) -> i32 {
        // strncat_s is not exported (not in the issue list); exercise the shared helper.
        ncat_s(d.as_mut_ptr(), d.len(), c(src).as_ptr(), n)
    }
    unsafe fn wcs_null_src_(d: &mut [u8]) -> i32 {
        ncat_s(d.as_mut_ptr(), d.len(), std::ptr::null(), 1)
    }

    #[test]
    fn searching() {
        unsafe {
            let a = c("hello, world");
            assert_eq!(strspn(p(&a), p(&c("helo"))), 5);
            assert_eq!(strspn(p(&a), p(&c(""))), 0);
            assert_eq!(strcspn(p(&a), p(&c(", "))), 5);
            assert_eq!(strcspn(p(&a), p(&c("xyz"))), 12);
            let r = strpbrk(p(&a), p(&c("ow")));
            assert_eq!(r as usize - p(&a) as usize, 4);
            assert!(strpbrk(p(&a), p(&c("xyz"))).is_null());
            assert!(strpbrk(p(&a), p(&c(""))).is_null());
        }
    }

    #[test]
    fn tokenizing() {
        unsafe {
            let mut a = c(",,a, b;;c,");
            let d = c(",; ");
            let mut got = vec![];
            let mut t = strtok(mp(&mut a), p(&d));
            while !t.is_null() {
                got.push(s(t));
                t = strtok(std::ptr::null_mut(), p(&d));
            }
            assert_eq!(got, ["a", "b", "c"]);
            assert!(strtok(std::ptr::null_mut(), p(&d)).is_null());
            let mut e = c(",,");
            assert!(strtok(mp(&mut e), p(&d)).is_null());

            let mut a = c("x y  z");
            let sp = c(" ");
            let mut ctx: *mut c_char = std::ptr::null_mut();
            let mut got = vec![];
            let mut t = strtok_s(mp(&mut a), p(&sp), &mut ctx);
            while !t.is_null() {
                got.push(s(t));
                t = strtok_s(std::ptr::null_mut(), p(&sp), &mut ctx);
            }
            assert_eq!(got, ["x", "y", "z"]);
            // errors
            assert!(strtok_s(std::ptr::null_mut(), p(&sp), std::ptr::null_mut()).is_null());
            let mut ctx2: *mut c_char = std::ptr::null_mut();
            assert!(strtok_s(std::ptr::null_mut(), p(&sp), &mut ctx2).is_null());
            assert!(strtok_s(mp(&mut a), std::ptr::null(), &mut ctx2).is_null());

            let mut a = w("é€ ü€x");
            let dl = w("€ ");
            let mut ctx: *mut u16 = std::ptr::null_mut();
            let t1 = wcstok_s(a.as_mut_ptr(), dl.as_ptr(), &mut ctx);
            assert_eq!(ws(std::slice::from_raw_parts(t1, 3)), "é");
            let t2 = wcstok_s(std::ptr::null_mut(), dl.as_ptr(), &mut ctx);
            assert_eq!(ws(std::slice::from_raw_parts(t2, 3)), "ü");
            let t3 = wcstok_s(std::ptr::null_mut(), dl.as_ptr(), &mut ctx);
            assert_eq!(ws(std::slice::from_raw_parts(t3, 2)), "x");
            assert!(wcstok_s(std::ptr::null_mut(), dl.as_ptr(), &mut ctx).is_null());
            assert!(wcstok_s(std::ptr::null_mut(), dl.as_ptr(), std::ptr::null_mut()).is_null());
        }
    }

    #[test]
    fn wcsdup() {
        unsafe {
            let src = w("héllo €");
            let d = _wcsdup(src.as_ptr());
            assert!(!d.is_null());
            assert_ne!(d as *const u16, src.as_ptr());
            assert_eq!(ws(std::slice::from_raw_parts(d, 8)), "héllo €");
            libc::free(d as *mut libc::c_void);
            assert!(_wcsdup(std::ptr::null()).is_null());
            let e = _wcsdup(w("").as_ptr());
            assert_eq!(*e, 0);
            libc::free(e as *mut libc::c_void);
        }
    }
}
