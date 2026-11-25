//! `api-ms-win-crt-utility`: `qsort`, `bsearch`, `rand`, `srand`, `rand_s`.
//!
//! The comparator is a win64 function, so `qsort` and `bsearch` are written here rather than
//! forwarded to glibc. `qsort` is an in-place heapsort (not stable, like the UCRT's quicksort;
//! O(n log n) worst case, no allocation). Invalid arguments (NULL base with a non-zero count,
//! zero element size, NULL comparator) set `EINVAL` and invoke the invalid parameter handler;
//! `bsearch` then returns NULL.
//!
//! `rand` uses per-thread state seeded with 1: `x = x * 214013 + 2531011`, result
//! `(x >> 16) & 0x7fff`. `rand_s` uses `getrandom`.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::invalid_parameter_err;
use std::cell::Cell;
use std::ffi::c_void;

const EINVAL: i32 = 22;

type Compare = unsafe extern "win64" fn(*const c_void, *const c_void) -> i32;

thread_local! {
    static SEED: Cell<u32> = const { Cell::new(1) };
}

unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    if a != b {
        std::ptr::swap_nonoverlapping(a, b, size);
    }
}

unsafe fn sift_down(base: *mut u8, size: usize, mut root: usize, end: usize, cmp: Compare) {
    loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end
            && cmp(
                base.add(child * size) as *const _,
                base.add((child + 1) * size) as *const _,
            ) < 0
        {
            child += 1;
        }
        if cmp(
            base.add(root * size) as *const _,
            base.add(child * size) as *const _,
        ) >= 0
        {
            return;
        }
        swap(base.add(root * size), base.add(child * size), size);
        root = child;
    }
}

win32! {
    module "api-ms-win-crt-utility" => "ucrtbase";
    Native fn qsort(base: *mut c_void, num: usize, size: usize, cmp: Option<Compare>) {
        if num == 0 {
            return;
        }
        let (true, false, true) = (!base.is_null(), size == 0, cmp.is_some()) else {
            invalid_parameter_err(EINVAL);
            return;
        };
        let Some(cmp) = cmp else { return };
        if num < 2 || num.checked_mul(size).is_none() {
            return;
        }
        let base = base as *mut u8;
        for i in (0..num / 2).rev() {
            sift_down(base, size, i, num, cmp);
        }
        for end in (1..num).rev() {
            swap(base, base.add(end * size), size);
            sift_down(base, size, 0, end, cmp);
        }
    }
    Native fn bsearch(key: *const c_void, base: *const c_void, num: usize, size: usize,
                      cmp: Option<Compare>) -> *mut c_void {
        let bad = cmp.is_none() || (num != 0 && (base.is_null() || size == 0 || key.is_null()));
        let Some(cmp) = cmp.filter(|_| !bad) else {
            invalid_parameter_err(EINVAL);
            return std::ptr::null_mut();
        };
        let (mut lo, mut hi) = (0usize, num);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let p = (base as *const u8).add(mid * size);
            let c = cmp(key, p as *const _);
            if c == 0 {
                return p as *mut c_void;
            }
            if c < 0 {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        std::ptr::null_mut()
    }
    Native fn srand(seed: u32) { SEED.with(|s| s.set(seed)) }
    Native fn rand() -> i32 {
        SEED.with(|s| {
            let x = s.get().wrapping_mul(214013).wrapping_add(2531011);
            s.set(x);
            ((x >> 16) & 0x7fff) as i32
        })
    }
    Native fn rand_s(out: *mut u32) -> i32 {
        if out.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        let mut v = 0u32;
        let n = libc::getrandom(&mut v as *mut u32 as *mut c_void, 4, 0);
        if n != 4 {
            *out = 0;
            return invalid_parameter_err(libc::EIO);
        }
        *out = v;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{errno, set_errno};

    unsafe extern "win64" fn cmp_i32(a: *const c_void, b: *const c_void) -> i32 {
        (*(a as *const i32)).cmp(&*(b as *const i32)) as i32
    }

    #[test]
    fn registered() {
        for n in ["qsort", "bsearch", "rand", "srand", "rand_s"] {
            assert!(crate::lookup("api-ms-win-crt-utility-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("ucrtbase", n).is_some());
        }
    }

    #[test]
    fn sorts_and_searches() {
        unsafe {
            let mut v: Vec<i32> = (0..1000).map(|i| (i * 7919 % 1009) - 500).collect();
            let mut want = v.clone();
            want.sort();
            qsort(v.as_mut_ptr() as *mut _, v.len(), 4, Some(cmp_i32));
            assert_eq!(v, want);
            // Sorted, reversed, equal, and tiny inputs.
            v.reverse();
            qsort(v.as_mut_ptr() as *mut _, v.len(), 4, Some(cmp_i32));
            assert_eq!(v, want);
            let mut same = [5i32; 9];
            qsort(same.as_mut_ptr() as *mut _, 9, 4, Some(cmp_i32));
            assert_eq!(same, [5; 9]);
            let mut one = [3i32];
            qsort(one.as_mut_ptr() as *mut _, 1, 4, Some(cmp_i32));
            qsort(std::ptr::null_mut(), 0, 4, Some(cmp_i32));
            // Odd element size (3 bytes, compared by first byte only).
            unsafe extern "win64" fn cmp_b(a: *const c_void, b: *const c_void) -> i32 {
                *(a as *const u8) as i32 - *(b as *const u8) as i32
            }
            let mut t = [[3u8, 1, 1], [1, 2, 2], [2, 3, 3]];
            qsort(t.as_mut_ptr() as *mut _, 3, 3, Some(cmp_b));
            assert_eq!(t, [[1, 2, 2], [2, 3, 3], [3, 1, 1]]);

            for k in [-500, 0, 123] {
                let r = bsearch(
                    &k as *const _ as *const _,
                    v.as_ptr() as *const _,
                    0,
                    4,
                    Some(cmp_i32),
                );
                assert!(r.is_null());
            }
            v.sort();
            let k = v[321];
            let r = bsearch(
                &k as *const _ as *const _,
                v.as_ptr() as *const _,
                v.len(),
                4,
                Some(cmp_i32),
            );
            assert_eq!(*(r as *const i32), k);
            let k = 10_000;
            assert!(bsearch(
                &k as *const _ as *const _,
                v.as_ptr() as *const _,
                v.len(),
                4,
                Some(cmp_i32)
            )
            .is_null());
        }
    }

    #[test]
    fn invalid_arguments() {
        unsafe {
            let mut x = [1i32, 2];
            let k = 1i32;
            set_errno(0);
            qsort(std::ptr::null_mut(), 2, 4, Some(cmp_i32));
            assert_eq!(errno(), EINVAL);
            set_errno(0);
            qsort(x.as_mut_ptr() as *mut _, 2, 0, Some(cmp_i32));
            assert_eq!(errno(), EINVAL);
            set_errno(0);
            qsort(x.as_mut_ptr() as *mut _, 2, 4, None);
            assert_eq!(errno(), EINVAL);
            set_errno(0);
            assert!(bsearch(
                &k as *const _ as *const _,
                x.as_ptr() as *const _,
                2,
                4,
                None
            )
            .is_null());
            assert_eq!(errno(), EINVAL);
            set_errno(0);
            assert!(bsearch(
                &k as *const _ as *const _,
                std::ptr::null(),
                2,
                4,
                Some(cmp_i32)
            )
            .is_null());
            assert_eq!(errno(), EINVAL);
            set_errno(0);
            assert!(bsearch(
                std::ptr::null(),
                x.as_ptr() as *const _,
                2,
                4,
                Some(cmp_i32)
            )
            .is_null());
            assert_eq!(errno(), EINVAL);
        }
    }

    #[test]
    fn rand_sequence_per_thread() {
        unsafe {
            // The well-known MSVC sequence for seed 1.
            let first: Vec<i32> = std::thread::spawn(|| (0..5).map(|_| rand()).collect())
                .join()
                .unwrap();
            assert_eq!(first, [41, 18467, 6334, 26500, 19169]);
            srand(1);
            assert_eq!(rand(), 41);
            srand(1);
            assert_eq!(rand(), 41);
            srand(42);
            let a = rand();
            std::thread::spawn(|| assert_eq!(rand(), 41))
                .join()
                .unwrap();
            srand(42);
            assert_eq!(rand(), a);
            for _ in 0..1000 {
                assert!((0..=0x7fff).contains(&rand()));
            }
        }
    }

    #[test]
    fn rand_s_works() {
        unsafe {
            let mut v = [0u32; 8];
            for x in v.iter_mut() {
                assert_eq!(rand_s(x), 0);
            }
            assert!(v.iter().any(|&x| x != v[0]));
            set_errno(0);
            assert_eq!(rand_s(std::ptr::null_mut()), EINVAL);
            assert_eq!(errno(), EINVAL);
        }
    }
}
