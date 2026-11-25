//! The formatter behind the UCRT `__stdio_common_v*printf` family (not a Windows module).
//!
//! Arguments come as a Win64 `va_list`: a pointer to 8-byte slots (integers, pointers and
//! doubles each take one slot; `long double` is a double). Output is built as a vector of code
//! units (`u32` holding a byte for narrow output, a UTF-16 unit for wide output) and the callers
//! in `api_ms_win_crt_stdio` copy it into their destination.
//!
//! Supported: `d i u o x X c C s S Z p e E f F g G a A %`, flags `- + space # 0`, width and
//! precision (also `*`), lengths `hh h l ll L w z j t I I32 I64`. `n` and unknown conversions
//! are invalid (`Err`), as in the UCRT. Positional parameters (`%1$d`) are not supported.
//!
//! Deviations: narrow/wide string conversions use UTF-8 (no ANSI code page); floating point
//! precision beyond 1100 digits is zero-filled (the exact binary expansion ends before that).
//! `%p` always prints 16 uppercase hex digits.

use std::fmt::Write as _;

/// `_CRT_INTERNAL_PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION`.
pub const LEGACY_VSPRINTF_NULL_TERMINATION: u64 = 1;
/// `_CRT_INTERNAL_PRINTF_STANDARD_SNPRINTF_BEHAVIOR`.
pub const STANDARD_SNPRINTF_BEHAVIOR: u64 = 2;
/// `_CRT_INTERNAL_PRINTF_LEGACY_WIDE_SPECIFIERS`: `%s`/`%c` follow the function's width.
pub const LEGACY_WIDE_SPECIFIERS: u64 = 4;
/// `_CRT_INTERNAL_PRINTF_LEGACY_MSVCRT_COMPATIBILITY`: floats carry at most 17 significant digits.
pub const LEGACY_MSVCRT_COMPATIBILITY: u64 = 8;
/// `_CRT_INTERNAL_PRINTF_LEGACY_THREE_DIGIT_EXPONENTS`.
pub const LEGACY_THREE_DIGIT_EXPONENTS: u64 = 16;

const MAX_FIELD: usize = 1 << 26;
const MAX_DIGITS: usize = 1100;

/// A Win64 `va_list`.
#[derive(Clone, Copy)]
pub struct VaList(*const u64);

impl VaList {
    pub fn new(p: *const u8) -> Self {
        VaList(p as *const u64)
    }
    /// # Safety
    /// The list must have another argument.
    pub unsafe fn next_u64(&mut self) -> u64 {
        let v = self.0.read_unaligned();
        self.0 = self.0.add(1);
        v
    }
    /// # Safety
    /// As `next_u64`.
    pub unsafe fn next_f64(&mut self) -> f64 {
        f64::from_bits(self.next_u64())
    }
}

/// A character unit of a string (`u8` or `u16`).
pub trait Unit: Copy + Default + PartialEq {
    const WIDE: bool;
    fn to_u32(self) -> u32;
    fn from_u32(v: u32) -> Self;
}
impl Unit for u8 {
    const WIDE: bool = false;
    fn to_u32(self) -> u32 {
        self as u32
    }
    fn from_u32(v: u32) -> Self {
        v as u8
    }
}
impl Unit for u16 {
    const WIDE: bool = true;
    fn to_u32(self) -> u32 {
        self as u32
    }
    fn from_u32(v: u32) -> Self {
        v as u16
    }
}

/// Reads a NUL-terminated string, at most `max` units.
///
/// # Safety
/// `p` must point to a readable string (or `max` readable units).
pub unsafe fn read_units<T: Unit>(p: *const T, max: usize) -> Vec<u32> {
    let mut v = Vec::new();
    while v.len() < max {
        let c = *p.add(v.len());
        if c == T::default() {
            break;
        }
        v.push(c.to_u32());
    }
    v
}

/// Converts units between narrow (UTF-8 bytes) and wide (UTF-16).
pub fn convert(src: &[u32], src_wide: bool, dst_wide: bool) -> Vec<u32> {
    match (src_wide, dst_wide) {
        (false, false) | (true, true) => src.to_vec(),
        (false, true) => {
            let bytes: Vec<u8> = src.iter().map(|&c| c as u8).collect();
            String::from_utf8_lossy(&bytes)
                .encode_utf16()
                .map(u32::from)
                .collect()
        }
        (true, false) => {
            let u: Vec<u16> = src.iter().map(|&c| c as u16).collect();
            String::from_utf16_lossy(&u)
                .bytes()
                .map(u32::from)
                .collect()
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Len {
    None,
    H,
    HH,
    L,
    LL,
    BigL,
    W,
    Z,
    I,
    I32,
    I64,
}

fn spaces(out: &mut Vec<u32>, n: usize) {
    out.extend(std::iter::repeat_n(b' ' as u32, n));
}

/// Writes `prefix` + `body` in a field of the spec's width. `zero_ok`: the `0` flag may pad.
fn field(out: &mut Vec<u32>, s: &Spec, prefix: &[u32], body: &[u32], zero_ok: bool) {
    let pad = s.width.saturating_sub(prefix.len() + body.len());
    if s.left {
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        spaces(out, pad);
    } else if s.zero && zero_ok {
        out.extend_from_slice(prefix);
        out.extend(std::iter::repeat_n(b'0' as u32, pad));
        out.extend_from_slice(body);
    } else {
        spaces(out, pad);
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
}

fn ascii(s: &str) -> Vec<u32> {
    s.bytes().map(u32::from).collect()
}

fn int_field(out: &mut Vec<u32>, s: &Spec, signed: bool, neg: bool, mag: u64, conv: u8) {
    let mut digits = match conv {
        b'o' => format!("{mag:o}"),
        b'x' => format!("{mag:x}"),
        b'X' => format!("{mag:X}"),
        _ => format!("{mag}"),
    };
    if s.prec == Some(0) && mag == 0 {
        digits.clear();
    }
    if let Some(p) = s.prec {
        if digits.len() < p {
            digits.insert_str(0, &"0".repeat(p.min(MAX_FIELD) - digits.len()));
        }
    }
    let mut prefix = String::new();
    if signed {
        if neg {
            prefix.push('-');
        } else if s.plus {
            prefix.push('+');
        } else if s.space {
            prefix.push(' ');
        }
    }
    if s.alt {
        match conv {
            b'o' if !digits.starts_with('0') => digits.insert(0, '0'),
            b'x' if mag != 0 => prefix.push_str("0x"),
            b'X' if mag != 0 => prefix.push_str("0X"),
            _ => {}
        }
    }
    field(out, s, &ascii(&prefix), &ascii(&digits), s.prec.is_none());
}

/// Digits (one per byte, `p + 1` of them) and decimal exponent of `abs` rounded to `p` decimals
/// in scientific notation.
fn sci(abs: f64, p: usize, legacy: bool) -> (Vec<u8>, i32) {
    let pe = p.min(if legacy { 16 } else { MAX_DIGITS });
    let s = format!("{:.*e}", pe, abs);
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let mut d: Vec<u8> = m.bytes().filter(|b| *b != b'.').collect();
    d.resize(p.min(MAX_FIELD) + 1, b'0');
    (d, e.parse().unwrap_or(0))
}

fn fmt_e(abs: f64, s: &Spec, upper: bool, opts: u64) -> String {
    let prec = s.prec.unwrap_or(6).min(MAX_FIELD);
    let (d, x) = sci(abs, prec, opts & LEGACY_MSVCRT_COMPATIBILITY != 0);
    let mut r = String::new();
    r.push(d[0] as char);
    if prec > 0 || s.alt {
        r.push('.');
    }
    r.extend(d[1..].iter().map(|&b| b as char));
    r.push(if upper { 'E' } else { 'e' });
    r.push(if x < 0 { '-' } else { '+' });
    let min = if opts & LEGACY_THREE_DIGIT_EXPONENTS != 0 {
        3
    } else {
        2
    };
    let _ = write!(r, "{:0min$}", x.unsigned_abs());
    r
}

fn fmt_f(abs: f64, s: &Spec, prec: usize, opts: u64) -> String {
    let prec = prec.min(MAX_FIELD);
    let mut r;
    let legacy = opts & LEGACY_MSVCRT_COMPATIBILITY != 0;
    let (d, x) = if legacy {
        sci(abs, 16, true)
    } else {
        (Vec::new(), 0)
    };
    if legacy && abs != 0.0 && (x as i64 + 1 + prec as i64) > 17 {
        let ds: String = d.iter().map(|&b| b as char).collect();
        let (int, frac) = if x >= 0 {
            let x = x as usize;
            (
                ds[..=x.min(16)].to_string() + &"0".repeat(x.saturating_sub(16)),
                ds[(x + 1).min(17)..].to_string(),
            )
        } else {
            ("0".to_string(), "0".repeat((-x - 1) as usize) + &ds)
        };
        let mut frac = frac;
        frac.truncate(prec);
        while frac.len() < prec {
            frac.push('0');
        }
        r = int;
        if prec > 0 {
            r.push('.');
            r.push_str(&frac);
        }
    } else {
        r = format!("{:.*}", prec.min(MAX_DIGITS), abs);
        if prec > MAX_DIGITS {
            r.push_str(&"0".repeat(prec - MAX_DIGITS));
        }
    }
    if prec == 0 && s.alt {
        r.push('.');
    }
    r
}

fn strip_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn fmt_g(abs: f64, s: &Spec, upper: bool, opts: u64) -> String {
    let p = match s.prec {
        None => 6,
        Some(0) => 1,
        Some(p) => p.min(MAX_FIELD),
    };
    let (_, x) = sci(abs, p - 1, opts & LEGACY_MSVCRT_COMPATIBILITY != 0);
    if x < -4 || x >= p as i32 {
        let e = fmt_e(
            abs,
            &Spec {
                prec: Some(p - 1),
                ..*s
            },
            upper,
            opts,
        );
        if s.alt {
            return e;
        }
        let at = e.find(['e', 'E']).unwrap_or(e.len());
        strip_zeros(&e[..at]) + &e[at..]
    } else {
        let f = fmt_f(abs, s, (p as i32 - 1 - x) as usize, opts);
        if s.alt {
            f
        } else {
            strip_zeros(&f)
        }
    }
}

/// `%a` without sign and without the `0x` prefix.
fn fmt_a(abs: f64, s: &Spec, upper: bool) -> String {
    let bits = abs.to_bits();
    let eb = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & ((1u64 << 52) - 1);
    let (mut lead, exp) = match (eb, mant) {
        (0, 0) => (0u64, 0),
        (0, _) => (0, -1022),
        _ => (1, eb - 1023),
    };
    let mut frac = String::new();
    match s.prec {
        None => {
            frac = format!("{mant:013x}").trim_end_matches('0').to_string();
        }
        Some(p) if p < 13 => {
            let shift = 4 * (13 - p) as u32;
            let full = (lead << 52) | mant;
            let mut q = full >> shift;
            let rem = full & ((1u64 << shift) - 1);
            let half = 1u64 << (shift - 1);
            if rem > half || (rem == half && q & 1 == 1) {
                q += 1;
            }
            lead = q >> (4 * p as u32);
            if p > 0 {
                frac = format!("{:0p$x}", q & ((1u64 << (4 * p as u32)) - 1));
            }
        }
        Some(p) => {
            frac = format!("{mant:013x}");
            frac.push_str(&"0".repeat(p.min(MAX_FIELD) - 13));
        }
    }
    let mut r = format!("{lead}");
    if !frac.is_empty() || s.alt {
        r.push('.');
    }
    r.push_str(&frac);
    let _ = write!(
        r,
        "p{}{}",
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    );
    if upper {
        r.make_ascii_uppercase();
    }
    r
}

fn float_field(out: &mut Vec<u32>, s: &Spec, v: f64, conv: u8, opts: u64) {
    let upper = conv.is_ascii_uppercase();
    let bits = v.to_bits();
    let neg = bits >> 63 != 0;
    let mut prefix = String::new();
    if neg {
        prefix.push('-');
    } else if s.plus {
        prefix.push('+');
    } else if s.space {
        prefix.push(' ');
    }
    let abs = v.abs();
    let body = if v.is_nan() {
        let mant = bits & ((1u64 << 52) - 1);
        let t = if mant >> 51 == 0 {
            "nan(snan)"
        } else if neg && mant == 1 << 51 {
            "nan(ind)"
        } else {
            "nan"
        };
        if upper {
            t.to_ascii_uppercase()
        } else {
            t.to_string()
        }
    } else if v.is_infinite() {
        if upper { "INF" } else { "inf" }.to_string()
    } else {
        match conv.to_ascii_lowercase() {
            b'e' => fmt_e(abs, s, upper, opts),
            b'f' => fmt_f(abs, s, s.prec.unwrap_or(6), opts),
            b'g' => fmt_g(abs, s, upper, opts),
            _ => {
                prefix.push_str(if upper { "0X" } else { "0x" });
                fmt_a(abs, s, upper)
            }
        }
    };
    field(out, s, &ascii(&prefix), &ascii(&body), v.is_finite());
}

/// Whether the source of a `%s`/`%c`-like conversion is wide.
fn source_wide(conv: u8, len: Len, wide_fn: bool, opts: u64) -> bool {
    match len {
        Len::H | Len::HH => false,
        Len::L | Len::LL | Len::W => true,
        _ => {
            let lower = wide_fn && opts & LEGACY_WIDE_SPECIFIERS != 0;
            if conv.is_ascii_lowercase() {
                lower
            } else {
                !lower
            }
        }
    }
}

/// Whether `%s`/`%c`-like conversions of the scanner or printer read wide data (shared helper).
pub fn string_is_wide(conv: u8, h: bool, l: bool, wide_fn: bool, legacy_wide: bool) -> bool {
    let len = if h {
        Len::H
    } else if l {
        Len::L
    } else {
        Len::None
    };
    source_wide(
        conv,
        len,
        wide_fn,
        if legacy_wide {
            LEGACY_WIDE_SPECIFIERS
        } else {
            0
        },
    )
}

/// Formats `fmt` (units without the terminating NUL). `wide_fn`: the function is a `w` one.
/// `Err(())` means an invalid format (the caller raises the invalid parameter handler).
///
#[allow(clippy::result_unit_err)]
/// # Safety
/// `va` must hold the arguments the format asks for, with valid pointers for `s`, `Z`.
pub unsafe fn format(
    fmt: &[u32],
    wide_fn: bool,
    opts: u64,
    va: &mut VaList,
) -> Result<Vec<u32>, ()> {
    let mut out = Vec::with_capacity(fmt.len() + 16);
    let n = fmt.len();
    let mut i = 0;
    let at = |i: usize| -> u32 {
        if i < n {
            fmt[i]
        } else {
            0
        }
    };
    while i < n {
        if fmt[i] != b'%' as u32 {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        let mut s = Spec::default();
        loop {
            match at(i) {
                0x2d => s.left = true,
                0x2b => s.plus = true,
                0x20 => s.space = true,
                0x23 => s.alt = true,
                0x30 => s.zero = true,
                _ => break,
            }
            i += 1;
        }
        if at(i) == b'*' as u32 {
            i += 1;
            let w = va.next_u64() as i32;
            if w < 0 {
                s.left = true;
            }
            s.width = (w.unsigned_abs() as usize).min(MAX_FIELD);
        } else {
            while (0x30..=0x39).contains(&at(i)) {
                s.width = (s.width * 10 + (at(i) - 0x30) as usize).min(MAX_FIELD);
                i += 1;
            }
        }
        if at(i) == b'.' as u32 {
            i += 1;
            if at(i) == b'*' as u32 {
                i += 1;
                let p = va.next_u64() as i32;
                s.prec = if p < 0 { None } else { Some(p as usize) };
            } else {
                let mut p = 0usize;
                while (0x30..=0x39).contains(&at(i)) {
                    p = (p * 10 + (at(i) - 0x30) as usize).min(MAX_FIELD);
                    i += 1;
                }
                s.prec = Some(p);
            }
        }
        let mut len = Len::None;
        loop {
            let c = at(i);
            len = match (c, len) {
                (0x68, Len::H) => Len::HH,
                (0x68, _) => Len::H,
                (0x6c, Len::L) => Len::LL,
                (0x6c, _) => Len::L,
                (0x4c, _) => Len::BigL,
                (0x77, _) => Len::W,
                (0x7a, _) | (0x6a, _) | (0x74, _) => Len::Z,
                (0x49, _) => {
                    if at(i + 1) == 0x36 && at(i + 2) == 0x34 {
                        i += 2;
                        Len::I64
                    } else if at(i + 1) == 0x33 && at(i + 2) == 0x32 {
                        i += 2;
                        Len::I32
                    } else {
                        Len::I
                    }
                }
                _ => break,
            };
            i += 1;
        }
        if i >= n {
            return Err(());
        }
        let conv = fmt[i];
        i += 1;
        if conv > 0x7f {
            return Err(());
        }
        let conv = conv as u8;
        match conv {
            b'%' => out.push(b'%' as u32),
            b'd' | b'i' => {
                let raw = va.next_u64();
                let v: i64 = match len {
                    Len::HH => raw as i8 as i64,
                    Len::H => raw as i16 as i64,
                    Len::LL | Len::Z | Len::I | Len::I64 => raw as i64,
                    _ => raw as i32 as i64,
                };
                int_field(&mut out, &s, true, v < 0, v.unsigned_abs(), b'd');
            }
            b'u' | b'o' | b'x' | b'X' => {
                let raw = va.next_u64();
                let v: u64 = match len {
                    Len::HH => raw as u8 as u64,
                    Len::H => raw as u16 as u64,
                    Len::LL | Len::Z | Len::I | Len::I64 => raw,
                    _ => raw as u32 as u64,
                };
                int_field(&mut out, &s, false, false, v, conv);
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let v = va.next_f64();
                float_field(&mut out, &s, v, conv, opts);
            }
            b'p' => {
                let v = va.next_u64();
                let body = ascii(&format!("{v:016X}"));
                field(&mut out, &Spec { zero: false, ..s }, &[], &body, false);
            }
            b'c' | b'C' => {
                let w = source_wide(conv, len, wide_fn, opts);
                let v = va.next_u64() as u32;
                let v = if w { v & 0xffff } else { v & 0xff };
                let body = if !w && wide_fn && v >= 0x80 {
                    vec![v]
                } else {
                    convert(&[v], w, wide_fn)
                };
                field(&mut out, &s, &[], &body, true);
            }
            b's' | b'S' => {
                let w = source_wide(conv, len, wide_fn, opts);
                let p = va.next_u64() as usize;
                let max = s.prec.unwrap_or(usize::MAX);
                let units = if p == 0 {
                    ascii("(null)").into_iter().take(max).collect::<Vec<_>>()
                } else if w {
                    read_units(p as *const u16, max)
                } else {
                    read_units(p as *const u8, max)
                };
                let units = if p == 0 {
                    units
                } else {
                    convert(&units, w, wide_fn)
                };
                field(&mut out, &s, &[], &units, true);
            }
            b'Z' => {
                let p = va.next_u64() as usize;
                let w = len == Len::W;
                let mut units = ascii("(null)");
                if p != 0 {
                    let len16 = (p as *const u16).read_unaligned() as usize;
                    let buf = ((p + 8) as *const usize).read_unaligned();
                    if buf != 0 {
                        units = if w {
                            let mut v = Vec::new();
                            for k in 0..len16 / 2 {
                                v.push((buf as *const u16).add(k).read_unaligned() as u32);
                            }
                            convert(&v, true, wide_fn)
                        } else {
                            let mut v = Vec::new();
                            for k in 0..len16 {
                                v.push(*(buf as *const u8).add(k) as u32);
                            }
                            convert(&v, false, wide_fn)
                        };
                    }
                }
                units.truncate(s.prec.unwrap_or(usize::MAX));
                field(&mut out, &s, &[], &units, true);
            }
            _ => return Err(()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[u64]) -> Vec<u64> {
        v.to_vec()
    }

    fn fmt_with(f: &str, wide: bool, opts: u64, a: &[u64]) -> Result<String, ()> {
        let a = args(a);
        let mut va = VaList::new(a.as_ptr() as *const u8);
        let fu: Vec<u32> = if wide {
            f.encode_utf16().map(u32::from).collect()
        } else {
            f.bytes().map(u32::from).collect()
        };
        let r = unsafe { format(&fu, wide, opts, &mut va) }?;
        Ok(if wide {
            let u: Vec<u16> = r.iter().map(|&c| c as u16).collect();
            String::from_utf16_lossy(&u)
        } else {
            let b: Vec<u8> = r.iter().map(|&c| c as u8).collect();
            String::from_utf8_lossy(&b).into_owned()
        })
    }

    fn f(fmt: &str, a: &[u64]) -> String {
        fmt_with(fmt, false, 0, a).unwrap()
    }

    fn d(x: f64) -> u64 {
        x.to_bits()
    }

    #[test]
    fn integers() {
        assert_eq!(f("%d", &[1]), "1");
        assert_eq!(f("%d", &[(-5i32) as u32 as u64]), "-5");
        assert_eq!(f("%I64d", &[0xffffffffu64 * 0xffffffff]), "-8589934591");
        assert_eq!(f("%+8I64d", &[100]), "    +100");
        assert_eq!(f("%+.8I64d", &[100]), "+00000100");
        assert_eq!(f("%+10.8I64d", &[100]), " +00000100");
        assert_eq!(f("%-1.5I64d", &[(-100i64) as u64]), "-00100");
        assert_eq!(f("%05I64d", &[(-100i64) as u64]), "-0100");
        assert_eq!(f("% I64d", &[100]), " 100");
        assert_eq!(f("% 8.5I64d", &[(-100i64) as u64]), "  -00100");
        assert_eq!(f("%.0I64d", &[0]), "");
        assert_eq!(
            f("%#+21.18I64x", &[(-100i64) as u64]),
            " 0x00ffffffffffffff9c"
        );
        assert_eq!(
            f("%#.25I64o", &[(-100i64) as u64]),
            "0001777777777777777777634"
        );
        assert_eq!(
            f("%#+18.21I64X", &[(-100i64) as u64]),
            "0X00000FFFFFFFFFFFFFF9C"
        );
        assert_eq!(f("%#+25.22I64u", &[u64::MAX]), "   0018446744073709551615");
        assert_eq!(f("%#-+ 08.5I64d", &[100]), "+00100  ");
        assert_eq!(f("%#012x", &[1]), "0x0000000001");
        assert_eq!(f("%#012x", &[0]), "000000000000");
        assert_eq!(f("%#04.8x", &[1]), "0x00000001");
        assert_eq!(f("%#04.8x", &[0]), "00000000");
        assert_eq!(f("%#-08.2x", &[1]), "0x01    ");
        assert_eq!(f("%#-08.2x", &[0]), "00      ");
        assert_eq!(f("%#.0x", &[1]), "0x1");
        assert_eq!(f("%#.0x", &[0]), "");
        assert_eq!(f("%#08o", &[1]), "00000001");
        assert_eq!(f("%#o", &[1]), "01");
        assert_eq!(f("%#o", &[0]), "0");
        assert_eq!(f("%#.0o", &[0]), "0");
        assert_eq!(f("%.80d", &[1]), format!("{}1", "0".repeat(79)));
        assert_eq!(
            f(
                "%hhd %hd %ld %lld",
                &[0x1ff, 0x1ffff, 0x1_0000_0003, 1 << 40]
            ),
            "-1 -1 3 1099511627776"
        );
        assert_eq!(f("%u", &[u32::MAX as u64]), "4294967295");
        assert_eq!(
            f("%I32d %Id %zd", &[(1 << 32) | 7, 1 << 33, 1 << 34]),
            "7 8589934592 17179869184"
        );
        assert_eq!(f("%*d|%-*d|%.*d", &[5, 42, 4, 7, 3, 9]), "   42|7   |009");
        assert_eq!(f("%*d", &[(-4i32) as u32 as u64, 1]), "1   ");
        assert_eq!(f("%+ d", &[1]), "+1");
        assert_eq!(f("100%%", &[]), "100%");
    }

    #[test]
    fn chars_and_strings() {
        let s = b"foo\0";
        let p = s.as_ptr() as u64;
        assert_eq!(f("%04s", &[p]), "0foo");
        assert_eq!(f("%.1s", &[p]), "f");
        assert_eq!(f("%-5s|%5s", &[p, p]), "foo  |  foo");
        assert_eq!(f("%s", &[0]), "(null)");
        assert_eq!(f("%04c", &[b'1' as u64]), "0001");
        assert_eq!(f("%-04c", &[b'1' as u64]), "1   ");
        let w: Vec<u16> = "wide\0".encode_utf16().collect();
        let wp = w.as_ptr() as u64;
        assert_eq!(f("%S", &[wp]), "wide");
        assert_eq!(f("%ws", &[wp]), "wide");
        assert_eq!(f("%-10ws|", &[wp]), "wide      |");
        assert_eq!(f("%10ls", &[wp]), "      wide");
        assert_eq!(f("%#+ -03whlls", &[wp]), "wide");
        assert_eq!(f("%lc%C", &[b'a' as u64, b'b' as u64]), "ab");
        // wide functions: legacy specifiers make %s wide, ISO make it narrow.
        assert_eq!(
            fmt_with("%s", true, LEGACY_WIDE_SPECIFIERS, &[wp]).unwrap(),
            "wide"
        );
        assert_eq!(fmt_with("%s", true, 0, &[p]).unwrap(), "foo");
        assert_eq!(fmt_with("%S", true, 0, &[wp]).unwrap(), "wide");
        assert_eq!(fmt_with("%hs|%ls", true, 0, &[p, wp]).unwrap(), "foo|wide");
        assert_eq!(
            fmt_with("%c", true, LEGACY_WIDE_SPECIFIERS, &[0x20ac]).unwrap(),
            "\u{20ac}"
        );
        // %Z: ANSI_STRING and UNICODE_STRING.
        let ansi = (4u16, 4u16, b"ansi".as_ptr());
        assert_eq!(f("%Z", &[&ansi as *const _ as u64]), "ansi");
        let wb: Vec<u16> = "wide".encode_utf16().collect();
        let uni = (8u16, 8u16, wb.as_ptr());
        assert_eq!(f("%wZ", &[&uni as *const _ as u64]), "wide");
    }

    #[test]
    fn pointers() {
        assert_eq!(f("%p", &[0x1234abcd]), "00000000".to_string() + "1234ABCD");
        assert_eq!(f("%p", &[0]), "0000000000000000");
        assert_eq!(f("%20p|", &[0xff]), "    00000000000000FF|");
    }

    #[test]
    fn floats_basic() {
        assert_eq!(f("%f", &[d(1.5)]), "1.500000");
        assert_eq!(f("%.0f", &[d(0.5)]), "0");
        assert_eq!(f("%.0f", &[d(1.5)]), "2");
        assert_eq!(f("%.0f", &[d(2.5)]), "2");
        assert_eq!(f("%#.0f", &[d(2.0)]), "2.");
        assert_eq!(f("%.2f", &[d(2.675)]), "2.67");
        assert_eq!(f("%f", &[d(-0.0)]), "-0.000000");
        assert_eq!(
            f("%8.3f|%-8.3f|%08.3f", &[d(3.1417), d(3.1417), d(-3.1417)]),
            "   3.142|3.142   |-003.142"
        );
        assert_eq!(f("%+f % f", &[d(1.0), d(1.0)]), "+1.000000  1.000000");
        assert_eq!(f("%f", &[d(1e20)]), "100000000000000000000.000000");
        assert_eq!(f("%f", &[d(1e300)]).len(), 301 + 7);
        assert_eq!(f("%.20f", &[d(0.1)]), "0.10000000000000000555");
        assert_eq!(f("%e", &[d(789456123.0)]), "7.894561e+08");
        assert_eq!(f("%E", &[d(0.000123)]), "1.230000E-04");
        assert_eq!(f("%+#23.15e", &[d(789456123.0)]), " +7.894561230000000e+08");
        assert_eq!(
            f("%-#23.15e|", &[d(789456123.0)]),
            "7.894561230000000e+08  |"
        );
        assert_eq!(f("%.0e", &[d(5.0)]), "5e+00");
        assert_eq!(f("%#.0e", &[d(5.0)]), "5.e+00");
        assert_eq!(f("%e", &[d(1e100)]), "1.000000e+100");
        assert_eq!(f("%e", &[d(0.0)]), "0.000000e+00");
        assert_eq!(
            fmt_with("%e", false, LEGACY_THREE_DIGIT_EXPONENTS, &[d(1.0)]).unwrap(),
            "1.000000e+000"
        );
        assert_eq!(f("%g", &[d(100000.0)]), "100000");
        assert_eq!(f("%g", &[d(1000000.0)]), "1e+06");
        assert_eq!(f("%g", &[d(0.0001)]), "0.0001");
        assert_eq!(f("%g", &[d(0.00001)]), "1e-05");
        assert_eq!(f("%g", &[d(1.5)]), "1.5");
        assert_eq!(f("%G", &[d(1e-10)]), "1E-10");
        assert_eq!(f("%#g", &[d(1.5)]), "1.50000");
        assert_eq!(f("%#1.1g", &[d(789456123.0)]), "8.e+08");
        assert_eq!(f("%.3g", &[d(1234.5)]), "1.23e+03");
        assert_eq!(f("%.10g", &[d(0.1)]), "0.1");
        assert_eq!(f("%g", &[d(0.0)]), "0");
        assert_eq!(f("%.0g", &[d(9.5)]), "1e+01");
    }

    #[test]
    fn floats_special_and_hex() {
        assert_eq!(f("%f", &[d(f64::INFINITY)]), "inf");
        assert_eq!(f("%F", &[d(f64::NEG_INFINITY)]), "-INF");
        assert_eq!(f("%08f", &[d(f64::INFINITY)]), "     inf");
        assert_eq!(f("%+f", &[d(f64::INFINITY)]), "+inf");
        assert_eq!(f("%f", &[0x7ff8000000000000]), "nan");
        assert_eq!(f("%f", &[0xfff8000000000000]), "-nan(ind)");
        assert_eq!(f("%E", &[0xfff8000000000000]), "-NAN(IND)");
        assert_eq!(f("%f", &[0xfff8000000000001]), "-nan");
        assert_eq!(f("%f", &[0x7ff0000000000001]), "nan(snan)");
        assert_eq!(f("%a", &[d(1.0)]), "0x1p+0");
        assert_eq!(f("%a", &[d(3.0)]), "0x1.8p+1");
        assert_eq!(f("%A", &[d(-0.5)]), "-0X1P-1");
        assert_eq!(f("%a", &[d(0.0)]), "0x0p+0");
        assert_eq!(f("%.2a", &[d(1.0)]), "0x1.00p+0");
        assert_eq!(f("%.0a", &[d(1.5)]), "0x2p+0");
        assert_eq!(f("%.1a", &[d(1.03125)]), "0x1.0p+0");
        assert_eq!(f("%#a", &[d(1.0)]), "0x1.p+0");
        assert_eq!(f("%12a|", &[d(1.0)]), "      0x1p+0|");
        assert_eq!(f("%012a|", &[d(1.0)]), "0x0000001p+0|");
        assert_eq!(f("%a", &[d(f64::from_bits(1))]), "0x0.0000000000001p-1022");
        assert_eq!(f("%a", &[d(f64::MAX)]), "0x1.fffffffffffffp+1023");
    }

    #[test]
    fn legacy_msvcrt_digits() {
        let o = LEGACY_MSVCRT_COMPATIBILITY;
        assert_eq!(
            fmt_with("%.20f", false, o, &[d(0.1)]).unwrap(),
            "0.10000000000000001000"
        );
        assert_eq!(
            fmt_with("%.25e", false, o, &[d(0.1)]).unwrap(),
            "1.0000000000000001000000000e-01"
        );
        assert_eq!(
            fmt_with("%f", false, o, &[d(1e20)]).unwrap(),
            "100000000000000000000.000000"
        );
    }

    #[test]
    fn invalid_formats() {
        assert!(fmt_with("%n", false, 0, &[0]).is_err());
        assert!(fmt_with("%", false, 0, &[]).is_err());
        assert!(fmt_with("%y", false, 0, &[1]).is_err());
        assert!(fmt_with("%5", false, 0, &[]).is_err());
        assert!(fmt_with("%_1I64d", false, 0, &[1]).is_err());
    }

    #[test]
    fn wide_output_and_conversion() {
        assert_eq!(
            fmt_with(
                "%d %s",
                true,
                LEGACY_WIDE_SPECIFIERS,
                &[5, "x\0".encode_utf16().collect::<Vec<_>>().as_ptr() as u64]
            )
            .unwrap(),
            "5 x"
        );
        let w = convert(&[0x20ac], true, false);
        assert_eq!(w, vec![0xe2, 0x82, 0xac]);
        assert_eq!(convert(&w, false, true), vec![0x20ac]);
    }
}
