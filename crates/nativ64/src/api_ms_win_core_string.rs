//! `MultiByteToWideChar` / `WideCharToMultiByte`.
//!
//! Code pages: `CP_UTF8` (65001) is exact. `CP_ACP` (0), `CP_OEMCP` (1), `CP_THREAD_ACP` (3) and
//! 1252 all resolve to Windows-1252, the ANSI page of the en-US locale the host reports
//! (the OEM page is deliberately not 437: the title only needs one single-byte page).
//! Other code pages fail with `ERROR_INVALID_PARAMETER`.

use crate::api_ms_win_core_errorhandling::set_last_error;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;
const MB_ERR_INVALID_CHARS: u32 = 8;
const WC_ERR_INVALID_CHARS: u32 = 0x80;
const CP_UTF8: u32 = 65001;

enum Page {
    Utf8,
    Cp1252,
}

fn page(cp: u32) -> Option<Page> {
    match cp {
        CP_UTF8 => Some(Page::Utf8),
        0 | 1 | 3 | 1252 => Some(Page::Cp1252),
        _ => None,
    }
}

/// Windows-1252 0x80..=0x9f; the five undefined bytes map to the C1 controls like Windows does.
const CP1252_HIGH: [u16; 32] = [
    0x20ac, 0x0081, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008d, 0x017d, 0x008f, 0x0090, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
    0x02dc, 0x2122, 0x0161, 0x203a, 0x0153, 0x009d, 0x017e, 0x0178,
];

fn cp1252_to_u16(b: u8) -> u16 {
    if (0x80..0xa0).contains(&b) {
        CP1252_HIGH[b as usize - 0x80]
    } else {
        b as u16
    }
}

fn u16_to_cp1252(u: u16) -> Option<u8> {
    if u < 0x80 || (0xa0..0x100).contains(&u) {
        Some(u as u8)
    } else {
        CP1252_HIGH
            .iter()
            .position(|&c| c == u)
            .map(|i| 0x80 + i as u8)
    }
}

/// The counted input: `len < 0` means NUL-terminated and the terminator is converted too.
unsafe fn input<'a, T>(p: *const T, len: i32, zero: impl Fn(&T) -> bool) -> Option<&'a [T]> {
    if p.is_null() || len == 0 || len < -1 {
        return None;
    }
    let n = if len < 0 {
        let mut n = 0;
        while !zero(&*p.add(n)) {
            n += 1;
        }
        n + 1
    } else {
        len as usize
    };
    Some(std::slice::from_raw_parts(p, n))
}

/// Writes `out` (or only measures when `cap == 0`) the way both converters do.
unsafe fn emit<T: Copy>(out: &[T], dst: *mut T, cap: i32) -> i32 {
    if cap == 0 {
        return out.len() as i32;
    }
    if dst.is_null() || cap < 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (cap as usize) < out.len() {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    std::ptr::copy_nonoverlapping(out.as_ptr(), dst, out.len());
    out.len() as i32
}

fn utf8_to_utf16(src: &[u8], strict: bool) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(src.len());
    let mut rest = src;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.extend(s.encode_utf16());
                break;
            }
            Err(e) => {
                let ok = e.valid_up_to();
                // SAFETY of unwrap: the prefix was validated.
                out.extend(std::str::from_utf8(&rest[..ok]).unwrap().encode_utf16());
                if strict {
                    return None;
                }
                out.push(0xfffd);
                rest = &rest[ok + e.error_len().unwrap_or(rest.len() - ok)..];
            }
        }
    }
    Some(out)
}

fn utf16_to_utf8(src: &[u16], strict: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len());
    for r in char::decode_utf16(src.iter().copied()) {
        match r {
            Ok(c) => out.extend(c.encode_utf8(&mut [0; 4]).as_bytes()),
            Err(_) if strict => return None,
            Err(_) => out.extend("\u{fffd}".as_bytes()),
        }
    }
    Some(out)
}

win32! {
    module "api-ms-win-core-string" => "kernel32";
    Native fn MultiByteToWideChar(code_page: u32, flags: u32, src: *const u8, src_len: i32,
                                  dst: *mut u16, dst_len: i32) -> i32 {
        let (Some(p), Some(s)) = (page(code_page), input(src, src_len, |b| *b == 0)) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        let out: Vec<u16> = match p {
            Page::Cp1252 => s.iter().map(|&b| cp1252_to_u16(b)).collect(),
            Page::Utf8 => match utf8_to_utf16(s, flags & MB_ERR_INVALID_CHARS != 0) {
                Some(v) => v,
                None => {
                    set_last_error(ERROR_NO_UNICODE_TRANSLATION);
                    return 0;
                }
            },
        };
        emit(&out, dst, dst_len)
    }
    // `default_char`/`used_default` apply to single-byte pages only (they must be NULL for UTF-8).
    // The default character is `?` unless given.
    Native fn WideCharToMultiByte(code_page: u32, flags: u32, src: *const u16, src_len: i32,
                                  dst: *mut u8, dst_len: i32, default_char: *const u8,
                                  used_default: *mut i32) -> i32 {
        let (Some(p), Some(s)) = (page(code_page), input(src, src_len, |u| *u == 0)) else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        if !used_default.is_null() {
            used_default.write_unaligned(0);
        }
        let out: Vec<u8> = match p {
            Page::Utf8 => {
                if !default_char.is_null() || !used_default.is_null() {
                    set_last_error(ERROR_INVALID_PARAMETER);
                    return 0;
                }
                match utf16_to_utf8(s, flags & WC_ERR_INVALID_CHARS != 0) {
                    Some(v) => v,
                    None => {
                        set_last_error(ERROR_NO_UNICODE_TRANSLATION);
                        return 0;
                    }
                }
            }
            Page::Cp1252 => {
                let def = if default_char.is_null() { b'?' } else { *default_char };
                let mut used = false;
                let v = s
                    .iter()
                    .map(|&u| {
                        u16_to_cp1252(u).unwrap_or_else(|| {
                            used = true;
                            def
                        })
                    })
                    .collect();
                if used && !used_default.is_null() {
                    used_default.write_unaligned(1);
                }
                v
            }
        };
        emit(&out, dst, dst_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;
    use std::ptr::{null, null_mut};

    #[test]
    fn utf8_to_wide() {
        unsafe {
            let s = "h\u{e9}\u{1f600}\0";
            assert_eq!(
                MultiByteToWideChar(65001, 0, s.as_ptr(), -1, null_mut(), 0),
                5
            );
            let mut b = [0u16; 8];
            assert_eq!(
                MultiByteToWideChar(65001, 0, s.as_ptr(), -1, b.as_mut_ptr(), 8),
                5
            );
            assert_eq!(&b[..5], &[0x68, 0xe9, 0xd83d, 0xde00, 0]);
            // counted: no terminator is added
            assert_eq!(
                MultiByteToWideChar(65001, 0, s.as_ptr(), 2, b.as_mut_ptr(), 8),
                2
            );
            // too small
            assert_eq!(
                MultiByteToWideChar(65001, 0, s.as_ptr(), -1, b.as_mut_ptr(), 3),
                0
            );
            assert_eq!(GetLastError(), 122);
        }
    }

    #[test]
    fn invalid_utf8() {
        unsafe {
            let bad = [b'a', 0xff, b'b', 0];
            let mut b = [0u16; 8];
            assert_eq!(
                MultiByteToWideChar(65001, 0, bad.as_ptr(), -1, b.as_mut_ptr(), 8),
                4
            );
            assert_eq!(&b[..4], &[0x61, 0xfffd, 0x62, 0]);
            assert_eq!(
                MultiByteToWideChar(65001, 8, bad.as_ptr(), -1, b.as_mut_ptr(), 8),
                0
            );
            assert_eq!(GetLastError(), 1113);
        }
    }

    #[test]
    fn bad_arguments() {
        unsafe {
            let mut b = [0u16; 2];
            assert_eq!(
                MultiByteToWideChar(65001, 0, null(), -1, b.as_mut_ptr(), 2),
                0
            );
            assert_eq!(GetLastError(), 87);
            assert_eq!(
                MultiByteToWideChar(65001, 0, b"a".as_ptr(), 0, b.as_mut_ptr(), 2),
                0
            );
            assert_eq!(GetLastError(), 87);
            assert_eq!(
                MultiByteToWideChar(932, 0, c"a".as_ptr().cast::<u8>(), -1, b.as_mut_ptr(), 2),
                0
            );
            assert_eq!(GetLastError(), 87);
            assert_eq!(
                MultiByteToWideChar(65001, 0, c"a".as_ptr().cast::<u8>(), -1, null_mut(), 2),
                0
            );
            assert_eq!(GetLastError(), 87);
        }
    }

    #[test]
    fn ansi_page() {
        unsafe {
            let src = [0x80u8, 0xe9, b'a', 0];
            let mut b = [0u16; 4];
            assert_eq!(
                MultiByteToWideChar(0, 0, src.as_ptr(), -1, b.as_mut_ptr(), 4),
                4
            );
            assert_eq!(b, [0x20ac, 0xe9, 0x61, 0]);
            let mut o = [0u8; 4];
            let mut used = 5;
            assert_eq!(
                WideCharToMultiByte(1252, 0, b.as_ptr(), 4, o.as_mut_ptr(), 4, null(), &mut used),
                4
            );
            assert_eq!(o, src);
            assert_eq!(used, 0);
            let w = [0x4e2du16, 0];
            assert_eq!(
                WideCharToMultiByte(0, 0, w.as_ptr(), -1, o.as_mut_ptr(), 4, null(), &mut used),
                2
            );
            assert_eq!(&o[..2], b"?\0");
            assert_eq!(used, 1);
        }
    }

    #[test]
    fn wide_to_utf8() {
        unsafe {
            let w = [0x68u16, 0xd83d, 0xde00, 0];
            assert_eq!(
                WideCharToMultiByte(65001, 0, w.as_ptr(), -1, null_mut(), 0, null(), null_mut()),
                6
            );
            let mut o = [0u8; 8];
            assert_eq!(
                WideCharToMultiByte(
                    65001,
                    0,
                    w.as_ptr(),
                    -1,
                    o.as_mut_ptr(),
                    8,
                    null(),
                    null_mut()
                ),
                6
            );
            assert_eq!(&o[..6], "h\u{1f600}\0".as_bytes());
            assert_eq!(
                WideCharToMultiByte(
                    65001,
                    0,
                    w.as_ptr(),
                    -1,
                    o.as_mut_ptr(),
                    3,
                    null(),
                    null_mut()
                ),
                0
            );
            assert_eq!(GetLastError(), 122);
            // lone surrogate
            let lone = [0xd800u16, 0];
            assert_eq!(
                WideCharToMultiByte(
                    65001,
                    0,
                    lone.as_ptr(),
                    -1,
                    o.as_mut_ptr(),
                    8,
                    null(),
                    null_mut()
                ),
                4
            );
            assert_eq!(
                WideCharToMultiByte(
                    65001,
                    0x80,
                    lone.as_ptr(),
                    -1,
                    o.as_mut_ptr(),
                    8,
                    null(),
                    null_mut()
                ),
                0
            );
            assert_eq!(GetLastError(), 1113);
            // UTF-8 forbids a default char
            assert_eq!(
                WideCharToMultiByte(
                    65001,
                    0,
                    w.as_ptr(),
                    -1,
                    o.as_mut_ptr(),
                    8,
                    b"?".as_ptr(),
                    null_mut()
                ),
                0
            );
            assert_eq!(GetLastError(), 87);
        }
    }
}
