//! `api-ms-win-crt-heap`: the CRT heap.
//!
//! Plain allocations forward to libc (`_wcsdup` of `api_ms_win_crt_string` allocates the same
//! way, so its result can be passed to `free`). A failed allocation sets the CRT `errno` to
//! `ENOMEM`.
//!
//! Aligned blocks are laid out as in the UCRT: a block of `size + align + 8` bytes is allocated
//! and the original pointer is stored in the 8 bytes right before the (8-aligned) address of the
//! returned block, which is where `_aligned_free` finds it. `_aligned_offset_malloc` aligns
//! `ptr + offset`. An alignment below 8 is raised to 8 (so alignment 0 works, as in the UCRT);
//! an alignment that is not a power of two sets `EINVAL`, invokes the invalid parameter handler
//! and returns NULL, as does `offset >= size` (unless `offset == 0`).
//!
//! The C++ `operator new` hooks are not modelled: `_callnewh` always returns 0 ("no new handler
//! installed, do not retry"), and `_set_new_mode` only records the mode (0 or 1; anything else
//! is `EINVAL`, returns -1).
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;

static NEW_MODE: AtomicI32 = AtomicI32::new(0);

fn checked(p: *mut c_void) -> *mut c_void {
    if p.is_null() {
        set_errno(ENOMEM);
    }
    p
}

unsafe fn aligned_alloc(size: usize, align: usize, offset: usize) -> *mut c_void {
    if !align.is_power_of_two() && align != 0 {
        invalid_parameter_err(EINVAL);
        return std::ptr::null_mut();
    }
    if offset != 0 && offset >= size {
        invalid_parameter_err(EINVAL);
        return std::ptr::null_mut();
    }
    let align = align.max(8);
    let Some(total) = size.checked_add(align + 8) else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    let raw = libc::malloc(total) as usize;
    if raw == 0 {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    }
    let ret = ((raw + 8 + offset + align - 1) & !(align - 1)) - offset;
    *(((ret & !7) - 8) as *mut usize) = raw;
    ret as *mut c_void
}

win32! {
    module "api-ms-win-crt-heap" => "ucrtbase";
    Native fn malloc(size: usize) -> *mut c_void { checked(libc::malloc(size)) }
    Native fn free(p: *mut c_void) { libc::free(p) }
    Native fn _free_base(p: *mut c_void) { libc::free(p) }
    Native fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
        let r = libc::realloc(p, size);
        // realloc(p, 0) frees the block and returns NULL: not a failure.
        if r.is_null() && size != 0 {
            set_errno(ENOMEM);
        }
        r
    }
    Native fn calloc(n: usize, size: usize) -> *mut c_void { checked(libc::calloc(n, size)) }
    Native fn _calloc_base(n: usize, size: usize) -> *mut c_void { checked(libc::calloc(n, size)) }
    Native fn _aligned_malloc(size: usize, align: usize) -> *mut c_void {
        aligned_alloc(size, align, 0)
    }
    Native fn _aligned_offset_malloc(size: usize, align: usize, offset: usize) -> *mut c_void {
        aligned_alloc(size, align, offset)
    }
    Native fn _aligned_free(p: *mut c_void) {
        if !p.is_null() {
            libc::free(*(((p as usize & !7) - 8) as *const *mut c_void));
        }
    }
    Native fn _set_new_mode(mode: i32) -> i32 {
        if mode != 0 && mode != 1 {
            invalid_parameter_err(EINVAL);
            return -1;
        }
        NEW_MODE.swap(mode, Ordering::Relaxed)
    }
    Native fn _callnewh(_size: usize) -> i32 { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::errno;

    #[test]
    fn registered_both_names() {
        for n in "malloc free realloc calloc _calloc_base _free_base _aligned_malloc _aligned_offset_malloc _aligned_free _set_new_mode _callnewh".split(' ') {
            assert!(crate::lookup("api-ms-win-crt-heap-l1-1-0.dll", n).is_some(), "{n}");
            assert!(crate::lookup("ucrtbase.dll", n).is_some(), "{n}");
        }
    }

    #[test]
    fn plain_heap() {
        unsafe {
            let p = malloc(16) as *mut u8;
            assert!(!p.is_null());
            p.write_bytes(7, 16);
            let p = realloc(p as *mut _, 64) as *mut u8;
            assert_eq!(*p.add(15), 7);
            free(p as *mut _);
            let c = calloc(4, 8) as *mut u8;
            assert!((0..32).all(|i| *c.add(i) == 0));
            _free_base(c as *mut _);
            let c = _calloc_base(2, 2);
            assert!(!c.is_null());
            free(c);
            assert!(realloc(malloc(8), 0).is_null());
            free(std::ptr::null_mut());
        }
    }

    #[test]
    fn failures_set_enomem() {
        unsafe {
            set_errno(0);
            assert!(malloc(usize::MAX).is_null());
            assert_eq!(errno(), ENOMEM);
            set_errno(0);
            assert!(calloc(usize::MAX, 2).is_null());
            assert_eq!(errno(), ENOMEM);
            set_errno(0);
            assert!(_aligned_malloc(usize::MAX, 16).is_null());
            assert_eq!(errno(), ENOMEM);
            set_errno(0);
            let p = malloc(8);
            assert!(realloc(p, usize::MAX).is_null());
            assert_eq!(errno(), ENOMEM);
            free(p);
        }
    }

    #[test]
    fn aligned() {
        unsafe {
            for align in [0usize, 1, 2, 4, 8, 16, 32, 64, 4096, 65536] {
                let p = _aligned_malloc(100, align);
                assert!(!p.is_null(), "{align}");
                assert_eq!(p as usize & align.saturating_sub(1), 0);
                (p as *mut u8).write_bytes(1, 100);
                _aligned_free(p);
            }
            for align in [3usize, 12, 100] {
                set_errno(0);
                assert!(_aligned_malloc(8, align).is_null());
                assert_eq!(errno(), EINVAL);
            }
            _aligned_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn aligned_offset() {
        unsafe {
            for (size, align, off) in [
                (100usize, 16usize, 4usize),
                (64, 64, 8),
                (40, 32, 0),
                (10, 8, 9),
            ] {
                let p = _aligned_offset_malloc(size, align, off);
                assert!(!p.is_null());
                assert_eq!((p as usize + off) & (align - 1), 0);
                (p as *mut u8).write_bytes(2, size);
                _aligned_free(p);
            }
            for (size, align, off) in [(16usize, 16usize, 16usize), (16, 16, 20), (16, 3, 0)] {
                set_errno(0);
                assert!(_aligned_offset_malloc(size, align, off).is_null());
                assert_eq!(errno(), EINVAL);
            }
            assert!(!_aligned_offset_malloc(0, 16, 0).is_null());
        }
    }

    #[test]
    fn new_mode_and_handler() {
        unsafe {
            assert_eq!(_set_new_mode(1), 0);
            assert_eq!(_set_new_mode(0), 1);
            set_errno(0);
            assert_eq!(_set_new_mode(2), -1);
            assert_eq!(errno(), EINVAL);
            assert_eq!(_set_new_mode(0), 0);
            assert_eq!(_callnewh(10), 0);
        }
    }
}
