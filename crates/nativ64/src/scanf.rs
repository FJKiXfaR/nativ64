//! The scanner behind the UCRT `__stdio_common_vsscanf` / `vswscanf` (not a Windows module).
//!
//! Conversions: `d i u o x X` (with `hh h l ll I64 I32 I z j t`), `e E f g G a A`, `s S c C`,
//! `[set]`, `p`, `n`, `%%`, assignment suppression `*` and field widths. Destination pointers
//! come from a Win64 `va_list` (see `printf::VaList`).
//!
//! Deviations: no `%n$`; floating point text is parsed with Rust (`inf`, `nan` accepted);
//! wide to narrow string storing truncates each unit to a byte.

use crate::printf::{string_is_wide, VaList};

/// `_CRT_INTERNAL_SCANF_SECURECRT`: `c`, `s` and `[` take a buffer size after the pointer.
pub const SECURECRT: u64 = 1;
/// `_CRT_INTERNAL_SCANF_LEGACY_WIDE_SPECIFIERS`.
pub const LEGACY_WIDE_SPECIFIERS: u64 = 2;

fn is_space(c: u32) -> bool {
    matches!(c, 9..=13 | 32)
}

fn digit_val(c: u32, base: u32) -> Option<u32> {
    char::from_u32(c)?.to_digit(base)
}

struct Input<'a> {
    s: &'a [u32],
    pos: usize,
}

impl Input<'_> {
    fn peek(&self) -> Option<u32> {
        self.s.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.pos += 1;
        }
    }
}

/// Stores a narrow or wide string at `dst`, with a terminator when `nul`.
unsafe fn store_units(dst: usize, wide: bool, units: &[u32], nul: bool) {
    if wide {
        let p = dst as *mut u16;
        for (k, &u) in units.iter().enumerate() {
            p.add(k).write_unaligned(u as u16);
        }
        if nul {
            p.add(units.len()).write_unaligned(0);
        }
    } else {
        let p = dst as *mut u8;
        for (k, &u) in units.iter().enumerate() {
            *p.add(k) = u as u8;
        }
        if nul {
            *p.add(units.len()) = 0;
        }
    }
}

unsafe fn store_int(dst: usize, bytes: usize, v: u64) {
    match bytes {
        1 => *(dst as *mut u8) = v as u8,
        2 => (dst as *mut u16).write_unaligned(v as u16),
        4 => (dst as *mut u32).write_unaligned(v as u32),
        _ => (dst as *mut u64).write_unaligned(v),
    }
}

/// Scans `input` with `fmt` (both without terminator). Returns the number of assigned fields,
/// or -1 when the input ends before the first conversion (and nothing was assigned).
///
/// # Safety
/// `va` must hold valid destination pointers for the format's conversions.
pub unsafe fn scan(input: &[u32], fmt: &[u32], wide_fn: bool, opts: u64, va: &mut VaList) -> i32 {
    let mut inp = Input { s: input, pos: 0 };
    let mut assigned = 0i32;
    let mut converted_any = false;
    let n = fmt.len();
    let mut i = 0;
    let at = |i: usize| -> u32 {
        if i < n {
            fmt[i]
        } else {
            0
        }
    };
    macro_rules! fail {
        () => {{
            return if assigned == 0 && !converted_any && inp.peek().is_none() {
                -1
            } else {
                assigned
            };
        }};
    }
    while i < n {
        let c = fmt[i];
        if is_space(c) {
            inp.skip_ws();
            i += 1;
            continue;
        }
        if c != b'%' as u32 {
            if inp.peek() == Some(c) {
                inp.pos += 1;
                i += 1;
                continue;
            }
            fail!();
        }
        i += 1;
        let suppress = at(i) == b'*' as u32;
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while (0x30..=0x39).contains(&at(i)) {
            width = (width * 10 + (at(i) - 0x30) as usize).min(1 << 30);
            i += 1;
        }
        let (mut h, mut l, mut big_l, mut ll) = (0, 0, false, false);
        let mut size: Option<usize> = None;
        loop {
            match at(i) {
                0x68 => h += 1,
                0x6c => {
                    l += 1;
                    if l == 2 {
                        ll = true;
                    }
                }
                0x4c => big_l = true,
                0x77 => l = 1,
                0x7a | 0x6a | 0x74 => ll = true,
                0x49 => {
                    if at(i + 1) == 0x36 && at(i + 2) == 0x34 {
                        i += 2;
                        ll = true;
                    } else if at(i + 1) == 0x33 && at(i + 2) == 0x32 {
                        i += 2;
                    } else {
                        ll = true;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        if i >= n {
            return assigned;
        }
        let conv = fmt[i];
        i += 1;
        let ch = if conv < 0x80 { conv as u8 } else { 0 };
        let maxw = if width == 0 { usize::MAX } else { width };
        let int_bytes = if h >= 2 {
            1
        } else if h == 1 {
            2
        } else if ll {
            8
        } else {
            4
        };
        let wide_dst = string_is_wide(
            ch,
            h > 0,
            l > 0,
            wide_fn,
            opts & LEGACY_WIDE_SPECIFIERS != 0,
        );
        match ch {
            b'%' => {
                inp.skip_ws();
                if inp.peek() == Some(b'%' as u32) {
                    inp.pos += 1;
                } else {
                    fail!();
                }
            }
            b'n' => {
                if !suppress {
                    store_int(va.next_u64() as usize, int_bytes, inp.pos as u64);
                }
            }
            b'c' | b'C' => {
                let w = width.max(1);
                if inp.pos + w > inp.s.len() {
                    fail!();
                }
                let units = &inp.s[inp.pos..inp.pos + w];
                inp.pos += w;
                converted_any = true;
                if !suppress {
                    let dst = va.next_u64() as usize;
                    if opts & SECURECRT != 0 {
                        size = Some(va.next_u64() as u32 as usize);
                    }
                    if size.is_some_and(|s| s < w) {
                        return assigned;
                    }
                    store_units(dst, wide_dst, units, false);
                    assigned += 1;
                }
            }
            b's' | b'S' | b'[' => {
                let mut set: Option<(Vec<u32>, bool)> = None;
                if ch == b'[' {
                    let neg = at(i) == b'^' as u32;
                    if neg {
                        i += 1;
                    }
                    let mut members = Vec::new();
                    if at(i) == b']' as u32 {
                        members.push(b']' as u32);
                        i += 1;
                    }
                    while i < n && fmt[i] != b']' as u32 {
                        if fmt[i] == b'-' as u32
                            && !members.is_empty()
                            && at(i + 1) != b']' as u32
                            && i + 1 < n
                        {
                            let lo = *members.last().unwrap();
                            let hi = fmt[i + 1];
                            members.extend(lo + 1..=hi.max(lo));
                            i += 2;
                        } else {
                            members.push(fmt[i]);
                            i += 1;
                        }
                    }
                    i += 1;
                    set = Some((members, neg));
                } else {
                    inp.skip_ws();
                }
                let start = inp.pos;
                while inp.pos - start < maxw {
                    let Some(c) = inp.peek() else { break };
                    let ok = match &set {
                        Some((m, neg)) => m.contains(&c) != *neg,
                        None => !is_space(c),
                    };
                    if !ok {
                        break;
                    }
                    inp.pos += 1;
                }
                if inp.pos == start {
                    fail!();
                }
                converted_any = true;
                if !suppress {
                    let dst = va.next_u64() as usize;
                    if opts & SECURECRT != 0 {
                        size = Some(va.next_u64() as u32 as usize);
                    }
                    let units = &inp.s[start..inp.pos];
                    if size.is_some_and(|s| s <= units.len()) {
                        return assigned;
                    }
                    store_units(dst, wide_dst, units, true);
                    assigned += 1;
                }
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                inp.skip_ws();
                let start = inp.pos;
                let mut neg = false;
                let mut taken = 0usize;
                if matches!(inp.peek(), Some(0x2b | 0x2d)) && taken < maxw {
                    neg = inp.peek() == Some(0x2d);
                    inp.pos += 1;
                    taken += 1;
                }
                let mut base = match ch {
                    b'o' => 8,
                    b'x' | b'X' | b'p' => 16,
                    b'i' => 0,
                    _ => 10,
                };
                let mut digits = 0;
                if (base == 0 || base == 16) && inp.peek() == Some(0x30) && taken < maxw {
                    let nx = inp.s.get(inp.pos + 1).copied();
                    if matches!(nx, Some(0x78 | 0x58)) && taken.saturating_add(2) <= maxw {
                        inp.pos += 2;
                        taken += 2;
                        base = 16;
                    } else if base == 0 {
                        base = 8;
                    }
                }
                if base == 0 {
                    base = 10;
                }
                let mut v: u64 = 0;
                while taken < maxw {
                    let Some(d) = inp.peek().and_then(|c| digit_val(c, base)) else {
                        break;
                    };
                    v = v.wrapping_mul(base as u64).wrapping_add(d as u64);
                    inp.pos += 1;
                    taken += 1;
                    digits += 1;
                }
                if digits == 0 {
                    if inp.pos > start
                        && base == 16
                        && inp.s.get(start..inp.pos).is_some_and(|s| s.contains(&0x30))
                    {
                        // "0x" with no digit after it: the "0" is the number.
                        v = 0;
                        inp.pos -= 1;
                    } else {
                        inp.pos = start;
                        fail!();
                    }
                }
                converted_any = true;
                if !suppress {
                    let v = if neg { v.wrapping_neg() } else { v };
                    let bytes = if ch == b'p' { 8 } else { int_bytes };
                    store_int(va.next_u64() as usize, bytes, v);
                    assigned += 1;
                }
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                inp.skip_ws();
                let start = inp.pos;
                let mut text = String::new();
                let lim = |inp: &Input, t: &String| {
                    inp.pos < inp.s.len() && inp.pos - start < maxw && t.len() < 4096
                };
                if lim(&inp, &text) && matches!(inp.s[inp.pos], 0x2b | 0x2d) {
                    text.push(inp.s[inp.pos] as u8 as char);
                    inp.pos += 1;
                }
                let rest: String = inp.s[inp.pos..]
                    .iter()
                    .take(3.min(maxw))
                    .map(|&c| char::from_u32(c).unwrap_or('?').to_ascii_lowercase())
                    .collect();
                if rest == "inf" || rest == "nan" {
                    text.push_str(&rest);
                    inp.pos += 3;
                } else {
                    let mut seen_dot = false;
                    let mut seen_exp = false;
                    let mut mant_digits = 0;
                    while lim(&inp, &text) {
                        let c = inp.s[inp.pos];
                        let chr = char::from_u32(c).unwrap_or('?');
                        if chr.is_ascii_digit() {
                            mant_digits += 1;
                        } else if chr == '.' && !seen_dot && !seen_exp {
                            seen_dot = true;
                        } else if (chr == 'e' || chr == 'E') && !seen_exp && mant_digits > 0 {
                            let nx = inp.s.get(inp.pos + 1).copied().and_then(char::from_u32);
                            let nx2 = inp.s.get(inp.pos + 2).copied().and_then(char::from_u32);
                            let ok = match nx {
                                Some(d) if d.is_ascii_digit() => true,
                                Some('+' | '-') => nx2.is_some_and(|d| d.is_ascii_digit()),
                                _ => false,
                            };
                            if !ok {
                                break;
                            }
                            seen_exp = true;
                            text.push('e');
                            inp.pos += 1;
                            if matches!(inp.s[inp.pos], 0x2b | 0x2d) {
                                text.push(inp.s[inp.pos] as u8 as char);
                                inp.pos += 1;
                            }
                            continue;
                        } else {
                            break;
                        }
                        text.push(chr);
                        inp.pos += 1;
                    }
                    if mant_digits == 0 {
                        inp.pos = start;
                        fail!();
                    }
                }
                let v: f64 = text.parse().unwrap_or(0.0);
                converted_any = true;
                if !suppress {
                    let dst = va.next_u64() as usize;
                    if l > 0 || big_l {
                        (dst as *mut f64).write_unaligned(v);
                    } else {
                        (dst as *mut f32).write_unaligned(v as f32);
                    }
                    assigned += 1;
                }
            }
            _ => return assigned,
        }
    }
    assigned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, fmt: &str, opts: u64, dst: &[u64]) -> i32 {
        let i: Vec<u32> = input.bytes().map(u32::from).collect();
        let f: Vec<u32> = fmt.bytes().map(u32::from).collect();
        let mut va = VaList::new(dst.as_ptr() as *const u8);
        unsafe { scan(&i, &f, false, opts, &mut va) }
    }

    #[test]
    fn integers_and_strings() {
        let (mut a, mut b, mut c) = (0i32, 0u32, 0i64);
        let mut s = [0u8; 16];
        let r = run(
            "  42 0x1F junk -7",
            "%d %i %s %lld",
            0,
            &[
                &mut a as *mut _ as u64,
                &mut b as *mut _ as u64,
                s.as_mut_ptr() as u64,
                &mut c as *mut _ as u64,
            ],
        );
        assert_eq!((r, a, b, c), (4, 42, 31, -7));
        assert_eq!(&s[..5], b"junk\0");
        let r = run("-7", "%lld", 0, &[&mut c as *mut _ as u64]);
        assert_eq!((r, c), (1, -7));
    }

    #[test]
    fn floats_chars_sets_n() {
        let (mut f, mut d) = (0f32, 0f64);
        let mut ch = [0u8; 4];
        let mut set = [0u8; 8];
        let mut n = 0i32;
        let r = run(
            "1.5 -2.5e2xyz12abc!",
            "%f %lf %c%c%[0-9]%[^!]%n",
            0,
            &[
                &mut f as *mut _ as u64,
                &mut d as *mut _ as u64,
                ch.as_mut_ptr() as u64,
                ch[2..].as_mut_ptr() as u64,
                set.as_mut_ptr() as u64,
                set[4..].as_mut_ptr() as u64,
                &mut n as *mut _ as u64,
            ],
        );
        // after "-2.5e2": c='x' c='y', then "[0-9]" fails on 'z'.
        assert_eq!((r, f, d), (4, 1.5, -250.0));
        assert_eq!((ch[0], ch[2]), (b'x', b'y'));
    }

    #[test]
    fn suppression_width_literals_and_eof() {
        let (mut a, mut b) = (0i32, 0i32);
        assert_eq!(run("12 34", "%*d %d", 0, &[&mut a as *mut _ as u64]), 1);
        assert_eq!(a, 34);
        assert_eq!(
            run(
                "12345",
                "%2d%3d",
                0,
                &[&mut a as *mut _ as u64, &mut b as *mut _ as u64]
            ),
            2
        );
        assert_eq!((a, b), (12, 345));
        assert_eq!(run("x=5", "x=%d", 0, &[&mut a as *mut _ as u64]), 1);
        assert_eq!(a, 5);
        assert_eq!(run("", "%d", 0, &[&mut a as *mut _ as u64]), -1);
        assert_eq!(run("abc", "%d", 0, &[&mut a as *mut _ as u64]), 0);
        assert_eq!(run("50%", "%d%%", 0, &[&mut a as *mut _ as u64]), 1);
    }

    #[test]
    fn secure_sizes() {
        let mut s = [0xffu8; 4];
        assert_eq!(
            run("abcdef", "%s", SECURECRT, &[s.as_mut_ptr() as u64, 4]),
            0
        );
        assert_eq!(run("abc", "%s", SECURECRT, &[s.as_mut_ptr() as u64, 4]), 1);
        assert_eq!(&s, b"abc\0");
    }
}
