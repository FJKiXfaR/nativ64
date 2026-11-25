//! `api-ms-win-crt-math`: the UCRT math functions.
//!
//! ABI notes (MSVC x64): `long` is 32-bit, so `lroundf` returns `i32`; `long double` is the
//! same as `double`; `int*` is `i32*`. Floats and doubles travel in XMM registers, which
//! `extern "win64"` handles for us.
//!
//! `errno` (see `api_ms_win_crt_runtime`): `sqrt`, `log`, `log2`, `log10`, `acos`, `asin` and `fmod` (and
//! their `f` variants) set `EDOM` on a domain error (finite or infinite operand, NaN
//! result); `log*(0)` sets `ERANGE`; `exp`/`expf` set `ERANGE` on overflow. Unsure against
//! Windows: the exact set of functions that report. Known gaps: the other functions (`pow`,
//! `sinh`, `exp2`, ...) do not set `errno`, and the handler registered with `__setusermatherr` is stored but never called.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::set_errno;
use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};

mod libm {
    use std::ffi::c_int;
    extern "C" {
        pub fn erff(x: f32) -> f32;
        pub fn nextafterf(x: f32, y: f32) -> f32;
        pub fn remainderf(x: f32, y: f32) -> f32;
        pub fn frexp(x: f64, exp: *mut c_int) -> f64;
        pub fn ldexp(x: f64, exp: c_int) -> f64;
    }
}

/// Handler stored by `__setusermatherr`; never invoked (see module docs).
static USER_MATH_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Currently registered `__setusermatherr` handler (0 when none).
pub fn user_math_handler() -> usize {
    USER_MATH_HANDLER.load(Ordering::Relaxed)
}

const EDOM: i32 = 33;
const ERANGE: i32 = 34;

/// Logarithm with the UCRT's errno: `EDOM` below zero, `ERANGE` at zero.
fn log_errno(x: f64) {
    if x < 0.0 {
        set_errno(EDOM);
    } else if x == 0.0 {
        set_errno(ERANGE);
    }
}

/// `EDOM` when a non-NaN operand produced a NaN.
fn dom_errno(nan_result: bool, operands_nan: bool) {
    if nan_result && !operands_nan {
        set_errno(EDOM);
    }
}

const FP_INFINITE: i16 = 1;
const FP_NAN: i16 = 2;
const FP_NORMAL: i16 = -1;
const FP_SUBNORMAL: i16 = -2;
const FP_ZERO: i16 = 0;

fn class_of(c: std::num::FpCategory) -> i16 {
    use std::num::FpCategory::*;
    match c {
        Nan => FP_NAN,
        Infinite => FP_INFINITE,
        Zero => FP_ZERO,
        Subnormal => FP_SUBNORMAL,
        Normal => FP_NORMAL,
    }
}

/// `nextafterf` toward a double target (`long double` == `double` on MSVC).
fn next_toward(x: f32, y: f64) -> f32 {
    if x.is_nan() || y.is_nan() {
        return x + y as f32;
    }
    let xd = x as f64;
    if xd == y {
        return y as f32;
    }
    let dir = if y > xd {
        f32::INFINITY
    } else {
        f32::NEG_INFINITY
    };
    unsafe { libm::nextafterf(x, dir) }
}

win32! {
    module "api-ms-win-crt-math" => "ucrtbase";
    Native fn __setusermatherr(handler: usize) {
        USER_MATH_HANDLER.store(handler, Ordering::Relaxed);
    }
    // Sign tests return 0x8000 when the sign bit is set (UCRT `_dsign`/`_fdsign`/`_ldsign`).
    Native fn _dsign(x: f64) -> c_int { if x.is_sign_negative() { 0x8000 } else { 0 } }
    Native fn _fdsign(x: f32) -> c_int { if x.is_sign_negative() { 0x8000 } else { 0 } }
    Native fn _ldsign(x: f64) -> c_int { if x.is_sign_negative() { 0x8000 } else { 0 } }
    // Classification takes a pointer (UCRT `_dtest(double*)`, `_fdtest(float*)`), returns a short.
    Native fn _dtest(x: *const f64) -> i16 {
        debug_assert!(!x.is_null());
        class_of((*x).classify())
    }
    Native fn _fdtest(x: *const f32) -> i16 {
        debug_assert!(!x.is_null());
        class_of((*x).classify())
    }
    Native fn _isnan(x: f64) -> c_int { x.is_nan() as c_int }
    Native fn acos(x: f64) -> f64 { let r = x.acos(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn asin(x: f64) -> f64 { let r = x.asin(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn atan(x: f64) -> f64 { x.atan() }
    Native fn ceil(x: f64) -> f64 { x.ceil() }
    Native fn cos(x: f64) -> f64 { x.cos() }
    Native fn cosh(x: f64) -> f64 { x.cosh() }
    Native fn exp(x: f64) -> f64 {
        let r = x.exp();
        if r.is_infinite() && x.is_finite() { set_errno(ERANGE); }
        r
    }
    Native fn exp2(x: f64) -> f64 { x.exp2() }
    Native fn expm1(x: f64) -> f64 { x.exp_m1() }
    Native fn floor(x: f64) -> f64 { x.floor() }
    Native fn log(x: f64) -> f64 { log_errno(x); x.ln() }
    Native fn log10(x: f64) -> f64 { log_errno(x); x.log10() }
    Native fn log1p(x: f64) -> f64 { x.ln_1p() }
    Native fn log2(x: f64) -> f64 { log_errno(x); x.log2() }
    Native fn sin(x: f64) -> f64 { x.sin() }
    Native fn sinh(x: f64) -> f64 { x.sinh() }
    Native fn sqrt(x: f64) -> f64 { let r = x.sqrt(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn tan(x: f64) -> f64 { x.tan() }
    Native fn acosf(x: f32) -> f32 { let r = x.acos(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn asinf(x: f32) -> f32 { let r = x.asin(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn atanf(x: f32) -> f32 { x.atan() }
    Native fn cbrtf(x: f32) -> f32 { x.cbrt() }
    Native fn ceilf(x: f32) -> f32 { x.ceil() }
    Native fn cosf(x: f32) -> f32 { x.cos() }
    Native fn erff(x: f32) -> f32 { libm::erff(x) }
    Native fn exp2f(x: f32) -> f32 { x.exp2() }
    Native fn expf(x: f32) -> f32 {
        let r = x.exp();
        if r.is_infinite() && x.is_finite() { set_errno(ERANGE); }
        r
    }
    Native fn log10f(x: f32) -> f32 { log_errno(x as f64); x.log10() }
    Native fn log2f(x: f32) -> f32 { log_errno(x as f64); x.log2() }
    Native fn logf(x: f32) -> f32 { log_errno(x as f64); x.ln() }
    Native fn sinf(x: f32) -> f32 { x.sin() }
    Native fn sinhf(x: f32) -> f32 { x.sinh() }
    Native fn sqrtf(x: f32) -> f32 { let r = x.sqrt(); dom_errno(r.is_nan(), x.is_nan()); r }
    Native fn tanf(x: f32) -> f32 { x.tan() }
    Native fn atan2(y: f64, x: f64) -> f64 { y.atan2(x) }
    Native fn atan2f(y: f32, x: f32) -> f32 { y.atan2(x) }
    Native fn fmod(x: f64, y: f64) -> f64 {
        let r = x % y;
        dom_errno(r.is_nan(), x.is_nan() || y.is_nan());
        r
    }
    Native fn fmodf(x: f32, y: f32) -> f32 {
        let r = x % y;
        dom_errno(r.is_nan(), x.is_nan() || y.is_nan());
        r
    }
    Native fn pow(x: f64, y: f64) -> f64 { x.powf(y) }
    Native fn powf(x: f32, y: f32) -> f32 { x.powf(y) }
    Native fn remainderf(x: f32, y: f32) -> f32 { libm::remainderf(x, y) }
    Native fn nextafterf(x: f32, y: f32) -> f32 { libm::nextafterf(x, y) }
    Native fn nexttowardf(x: f32, y: f64) -> f32 { next_toward(x, y) }
    // `int*` is 32-bit. A null exponent pointer is not checked by UCRT either.
    Native fn frexp(x: f64, exp: *mut i32) -> f64 {
        debug_assert!(!exp.is_null());
        libm::frexp(x, exp)
    }
    Native fn ldexp(x: f64, exp: i32) -> f64 { libm::ldexp(x, exp) }
    // Integral part is returned, fractional part stored (both keep the sign of `x`).
    Native fn modf(x: f64, int_part: *mut f64) -> f64 {
        debug_assert!(!int_part.is_null());
        let i = x.trunc();
        *int_part = i;
        if x.is_infinite() { 0.0_f64.copysign(x) } else { (x - i).copysign(x) }
    }
    Native fn modff(x: f32, int_part: *mut f32) -> f32 {
        debug_assert!(!int_part.is_null());
        let i = x.trunc();
        *int_part = i;
        if x.is_infinite() { 0.0_f32.copysign(x) } else { (x - i).copysign(x) }
    }
    // `long` is 32-bit: NaN and out-of-range values give 0x80000000 (i32::MIN), like the
    // x64 convert instruction the UCRT ends up using.
    Native fn lroundf(x: f32) -> i32 {
        let r = x.round();
        if (-2147483648.0..2147483648.0).contains(&r) { r as i32 } else { i32::MIN }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(name: &str) -> *const () {
        crate::lookup("api-ms-win-crt-math-l1-1-0.dll", name)
            .unwrap()
            .addr
    }

    #[test]
    fn registry_calls() {
        unsafe {
            let f: unsafe extern "win64" fn(f64) -> f64 = std::mem::transmute(get("sqrt"));
            assert_eq!(f(9.0), 3.0);
            let f: unsafe extern "win64" fn(f32, f32) -> f32 = std::mem::transmute(get("powf"));
            assert_eq!(f(2.0, 10.0), 1024.0);
            let f: unsafe extern "win64" fn(f32) -> i32 = std::mem::transmute(get("lroundf"));
            assert_eq!(f(-2.5), -3);
            let f: unsafe extern "win64" fn(*const f64) -> i16 = std::mem::transmute(get("_dtest"));
            assert_eq!(f(&f64::INFINITY), 1);
            let f: unsafe extern "win64" fn(f64, *mut i32) -> f64 =
                std::mem::transmute(get("frexp"));
            let mut e = 0;
            assert_eq!(f(8.0, &mut e), 0.5);
            assert_eq!(e, 4);
            let f: unsafe extern "win64" fn(f32, f64) -> f32 =
                std::mem::transmute(get("nexttowardf"));
            assert_eq!(f(1.0, 2.0), 1.0 + f32::EPSILON);
        }
        assert!(crate::lookup("ucrtbase", "cbrtf").is_some());
    }

    #[test]
    fn every_name_registered() {
        let names = "__setusermatherr _dsign _dtest _fdsign _fdtest _isnan _ldsign acos acosf asin asinf atan atan2 atan2f atanf cbrtf ceil ceilf cos cosf cosh erff exp exp2 exp2f expf expm1 floor fmod fmodf frexp ldexp log log10f log1p log2 log2f logf lroundf modf modff nextafterf nexttowardf pow powf remainderf sin sinf sinh sinhf sqrt sqrtf tan tanf";
        assert_eq!(names.split(' ').count(), 54);
        for n in names.split(' ') {
            assert!(crate::lookup("api-ms-win-crt-math", n).is_some(), "{n}");
        }
    }

    #[test]
    fn known_values() {
        unsafe {
            assert_eq!(floor(-1.5), -2.0);
            assert_eq!(ceilf(1.2), 2.0);
            assert_eq!(exp2(3.0), 8.0);
            assert_eq!(log2f(8.0), 3.0);
            assert_eq!(cbrtf(27.0), 3.0);
            assert_eq!(log10f(1000.0), 3.0);
            assert!((atan2(1.0, 1.0) - std::f64::consts::FRAC_PI_4).abs() < 1e-15);
            assert!((erff(0.5) - 0.5204999).abs() < 1e-6);
            assert_eq!(fmod(5.5, 2.0), 1.5);
            assert_eq!(fmodf(-5.5, 2.0), -1.5);
            assert_eq!(remainderf(5.0, 3.0), -1.0);
            assert_eq!(ldexp(0.5, 4), 8.0);
            assert_eq!(pow(2.0, 0.5), std::f64::consts::SQRT_2);
            assert_eq!(expm1(0.0), 0.0);
            assert_eq!(log1p(0.0), 0.0);
        }
    }

    #[test]
    fn special_values() {
        unsafe {
            assert!(sqrt(-1.0).is_nan());
            assert_eq!(log(0.0), f64::NEG_INFINITY);
            assert!(log(-1.0).is_nan());
            assert!(acos(2.0).is_nan());
            assert!(asinf(2.0).is_nan());
            assert_eq!(exp(f64::NEG_INFINITY), 0.0);
            assert_eq!(expf(f32::INFINITY), f32::INFINITY);
            assert!(fmod(1.0, 0.0).is_nan());
            assert!(fmod(f64::INFINITY, 1.0).is_nan());
            assert_eq!(pow(1.0, f64::NAN), 1.0);
            assert_eq!(pow(0.0, -1.0), f64::INFINITY);
            assert!(sqrtf(f32::NAN).is_nan());
            assert!(sqrt(-0.0).is_sign_negative());
            assert_eq!(atan(f64::INFINITY), std::f64::consts::FRAC_PI_2);
            assert!(sin(f64::INFINITY).is_nan());
            assert_eq!(atan2f(-0.0, -1.0), -std::f32::consts::PI);
        }
    }

    #[test]
    fn sign_and_class() {
        unsafe {
            assert_eq!(_dsign(-0.0), 0x8000);
            assert_eq!(_dsign(0.0), 0);
            assert_eq!(_fdsign(-1.0), 0x8000);
            assert_eq!(_fdsign(f32::INFINITY), 0);
            assert_eq!(_ldsign(f64::NEG_INFINITY), 0x8000);
            assert_ne!(_isnan(f64::NAN), 0);
            assert_eq!(_isnan(f64::INFINITY), 0);
            let sub64 = f64::MIN_POSITIVE / 2.0;
            let sub32 = f32::MIN_POSITIVE / 2.0;
            assert_eq!(_dtest(&f64::NAN), FP_NAN);
            assert_eq!(_dtest(&f64::NEG_INFINITY), FP_INFINITE);
            assert_eq!(_dtest(&-0.0), FP_ZERO);
            assert_eq!(_dtest(&sub64), FP_SUBNORMAL);
            assert_eq!(_dtest(&1.0), FP_NORMAL);
            assert_eq!(_fdtest(&f32::NAN), FP_NAN);
            assert_eq!(_fdtest(&f32::INFINITY), FP_INFINITE);
            assert_eq!(_fdtest(&0.0f32), FP_ZERO);
            assert_eq!(_fdtest(&sub32), FP_SUBNORMAL);
            assert_eq!(_fdtest(&1.0f32), FP_NORMAL);
        }
    }

    #[test]
    fn splitting() {
        unsafe {
            let mut e = 0;
            assert_eq!(frexp(0.0, &mut e), 0.0);
            assert_eq!(e, 0);
            assert_eq!(frexp(-12.0, &mut e), -0.75);
            assert_eq!(e, 4);
            let mut i = 0.0;
            assert_eq!(modf(-3.25, &mut i), -0.25);
            assert_eq!(i, -3.0);
            assert_eq!(modf(f64::INFINITY, &mut i), 0.0);
            assert_eq!(i, f64::INFINITY);
            assert!(modf(f64::NAN, &mut i).is_nan());
            let mut fi = 0.0f32;
            assert_eq!(modff(2.5, &mut fi), 0.5);
            assert_eq!(fi, 2.0);
            assert!(modff(-0.0, &mut fi).is_sign_negative());
        }
    }

    #[test]
    fn rounding_and_neighbours() {
        unsafe {
            assert_eq!(lroundf(0.5), 1);
            assert_eq!(lroundf(-0.5), -1);
            assert_eq!(lroundf(f32::NAN), i32::MIN);
            assert_eq!(lroundf(3.0e9), i32::MIN);
            assert_eq!(lroundf(-2147483648.0), i32::MIN);
            assert_eq!(nextafterf(1.0, 2.0), 1.0 + f32::EPSILON);
            assert_eq!(nextafterf(0.0, -1.0), -f32::from_bits(1));
            assert!(nextafterf(f32::NAN, 1.0).is_nan());
            assert_eq!(nexttowardf(1.0, 1.0), 1.0);
            assert_eq!(nexttowardf(1.0, 0.0), 1.0 - f32::EPSILON / 2.0);
            // Target between two floats: still moves one step.
            assert_eq!(nexttowardf(1.0, 1.0 + 1e-12), 1.0 + f32::EPSILON);
            assert!(nexttowardf(1.0, f64::NAN).is_nan());
        }
    }

    #[test]
    fn user_handler_is_stored() {
        unsafe { __setusermatherr(0x1234) };
        assert_eq!(user_math_handler(), 0x1234);
        unsafe { __setusermatherr(0) };
    }

    #[test]
    fn errno_on_domain_and_range_errors() {
        use crate::api_ms_win_crt_runtime::{errno, set_errno};
        unsafe {
            set_errno(0xdead);
            assert!(sqrt(-1.0).is_nan());
            assert_eq!(errno(), EDOM);
            set_errno(0xdead);
            assert_eq!(log(0.0), f64::NEG_INFINITY);
            assert_eq!(errno(), ERANGE);
            set_errno(0xdead);
            assert!(logf(-1.0).is_nan());
            assert_eq!(errno(), EDOM);
            set_errno(0xdead);
            assert!(acos(2.0).is_nan());
            assert_eq!(errno(), EDOM);
            set_errno(0xdead);
            assert!(fmod(1.0, 0.0).is_nan());
            assert_eq!(errno(), EDOM);
            set_errno(0xdead);
            assert_eq!(exp(1000.0), f64::INFINITY);
            assert_eq!(errno(), ERANGE);
            // No error: errno is left alone, also for NaN in, NaN out and infinite operands.
            set_errno(0xdead);
            assert_eq!(sqrt(4.0), 2.0);
            assert!(sqrt(f64::NAN).is_nan());
            assert_eq!(exp(f64::INFINITY), f64::INFINITY);
            assert_eq!(log(1.0), 0.0);
            assert_eq!(errno(), 0xdead);
        }
    }
}
