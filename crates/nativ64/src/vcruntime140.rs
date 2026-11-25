//! `vcruntime140`: the memory and string primitives the compiler's runtime exports.
//!
//! `memcmp` returns -1, 0 or 1. `memcpy` requires non-overlapping ranges, `memmove` does not.
//! A zero length never touches the pointers. The same functions are registered under `ucrtbase`,
//! which exports them too.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The handler `_purecall` would call; it is only recorded (nothing here calls it).
static PURECALL_HANDLER: AtomicUsize = AtomicUsize::new(0);

win32! {
    module "vcruntime140" => "ucrtbase";
    // Returns the previous handler (NULL at first).
    Native fn _set_purecall_handler(h: usize) -> usize {
        PURECALL_HANDLER.swap(h, Ordering::Relaxed)
    }
    Native fn _get_purecall_handler() -> usize {
        PURECALL_HANDLER.load(Ordering::Relaxed)
    }
    Native fn memchr(p: *const c_void, c: c_int, n: usize) -> *const c_void {
        if n == 0 {
            return std::ptr::null();
        }
        let s = std::slice::from_raw_parts(p as *const u8, n);
        match s.iter().position(|&b| b == c as u8) {
            Some(i) => (p as *const u8).add(i) as *const c_void,
            None => std::ptr::null(),
        }
    }
    Native fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        if n == 0 {
            return 0;
        }
        let x = std::slice::from_raw_parts(a as *const u8, n);
        let y = std::slice::from_raw_parts(b as *const u8, n);
        match x.cmp(y) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }
    Native fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        if n != 0 {
            std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, n);
        }
        dst
    }
    Native fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        if n != 0 {
            std::ptr::copy(src as *const u8, dst as *mut u8, n);
        }
        dst
    }
    Native fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
        if n != 0 {
            std::ptr::write_bytes(dst as *mut u8, c as u8, n);
        }
        dst
    }
    // The terminating NUL is part of the string: strchr(s, 0) points at it.
    Native fn strchr(s: *const c_char, c: c_int) -> *const c_char {
        let mut p = s;
        loop {
            if *p as u8 == c as u8 {
                return p;
            }
            if *p == 0 {
                return std::ptr::null();
            }
            p = p.add(1);
        }
    }
    Native fn strrchr(s: *const c_char, c: c_int) -> *const c_char {
        let mut p = s;
        let mut found = std::ptr::null();
        loop {
            if *p as u8 == c as u8 {
                found = p;
            }
            if *p == 0 {
                return found;
            }
            p = p.add(1);
        }
    }
    // Wide `strrchr`: `c` is a UTF-16 unit; the terminating NUL is part of the string.
    Native fn wcsrchr(s: *const u16, c: u16) -> *const u16 {
        let mut p = s;
        let mut found = std::ptr::null();
        loop {
            if *p == c {
                found = p;
            }
            if *p == 0 {
                return found;
            }
            p = p.add(1);
        }
    }
    // An empty needle matches at the start of the haystack.
    Native fn strstr(h: *const c_char, n: *const c_char) -> *const c_char {
        let nl = std::ffi::CStr::from_ptr(n).to_bytes();
        let hs = std::ffi::CStr::from_ptr(h).to_bytes();
        if nl.is_empty() {
            return h;
        }
        match hs.windows(nl.len()).position(|w| w == nl) {
            Some(i) => h.add(i),
            None => std::ptr::null(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purecall_handler_roundtrip() {
        unsafe {
            assert_eq!(_set_purecall_handler(0x1234), 0);
            assert_eq!(_get_purecall_handler(), 0x1234);
            assert_eq!(_set_purecall_handler(0), 0x1234);
        }
        assert!(crate::lookup("VCRUNTIME140.dll", "_set_purecall_handler").is_some());
    }

    #[test]
    fn registered_both_names() {
        for n in [
            "memchr", "memcmp", "memcpy", "memmove", "memset", "strchr", "strrchr", "strstr",
            "wcsrchr",
        ] {
            assert!(crate::lookup("VCRUNTIME140.dll", n).is_some(), "{n}");
            assert!(crate::lookup("ucrtbase", n).is_some(), "{n}");
        }
    }

    #[test]
    fn memory_functions() {
        unsafe {
            let mut a = [0u8; 8];
            assert_eq!(
                memset(a.as_mut_ptr().cast(), 0x1ff, 4),
                a.as_mut_ptr().cast()
            );
            assert_eq!(a, [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
            let src = [1u8, 2, 3, 4];
            memcpy(a.as_mut_ptr().cast(), src.as_ptr().cast(), 4);
            assert_eq!(&a[..4], &src);
            // Overlapping move forward and backward.
            memmove(a.as_mut_ptr().add(1).cast(), a.as_ptr().cast(), 4);
            assert_eq!(&a[..5], &[1, 1, 2, 3, 4]);
            memmove(a.as_mut_ptr().cast(), a.as_ptr().add(1).cast(), 4);
            assert_eq!(&a[..4], &[1, 2, 3, 4]);
            assert_eq!(memcmp(a.as_ptr().cast(), src.as_ptr().cast(), 4), 0);
            assert_eq!(
                memcmp(a.as_ptr().cast(), [1u8, 2, 4, 0].as_ptr().cast(), 4),
                -1
            );
            assert_eq!(memcmp([9u8].as_ptr().cast(), [1u8].as_ptr().cast(), 1), 1);
            let p = memchr(a.as_ptr().cast(), 3, 8);
            assert_eq!(p, a.as_ptr().add(2).cast());
            assert!(memchr(a.as_ptr().cast(), 3, 2).is_null());
            // Zero length with null pointers.
            let null = std::ptr::null_mut::<c_void>();
            assert!(memchr(null, 1, 0).is_null());
            assert_eq!(memcmp(null, null, 0), 0);
            assert_eq!(memcpy(null, null, 0), null);
            assert_eq!(memmove(null, null, 0), null);
            assert_eq!(memset(null, 0, 0), null);
        }
    }

    #[test]
    fn string_functions() {
        unsafe {
            let s = c"abcabc".as_ptr();
            assert_eq!(strchr(s, b'b' as c_int), s.add(1));
            assert_eq!(strrchr(s, b'b' as c_int), s.add(4));
            assert_eq!(strchr(s, 0), s.add(6));
            assert_eq!(strrchr(s, 0), s.add(6));
            let w: [u16; 7] = [97, 98, 99, 97, 98, 99, 0];
            assert_eq!(wcsrchr(w.as_ptr(), 98), w.as_ptr().add(4));
            assert_eq!(wcsrchr(w.as_ptr(), 0), w.as_ptr().add(6));
            assert!(wcsrchr(w.as_ptr(), 122).is_null());
            assert!(strchr(s, b'z' as c_int).is_null());
            assert!(strrchr(s, b'z' as c_int).is_null());
            assert_eq!(strstr(s, c"ca".as_ptr()), s.add(2));
            assert_eq!(strstr(s, c"".as_ptr()), s);
            assert!(strstr(s, c"abd".as_ptr()).is_null());
            assert!(strstr(c"ab".as_ptr(), c"abc".as_ptr()).is_null());
        }
    }
}
