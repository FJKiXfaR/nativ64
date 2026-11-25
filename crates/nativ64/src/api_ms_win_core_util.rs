//! `api-ms-win-core-util`: pointer encoding.
//!
//! `EncodePointer`/`DecodePointer` obfuscate a pointer with a per-process secret (a random
//! 64-bit cookie, drawn on first use): xor with the cookie, then rotate right by `cookie & 63`.
//! The scheme is reversible and consistent between the two calls, which is all that is
//! promised; the values differ from Windows'. `NULL` is encoded like any other value (the
//! result is not `NULL`), as on Windows. `EncodeSystemPointer`/`DecodeSystemPointer` use a fixed
//! system-wide key instead (the encoding is identical in every process of this loader).
//!
//! Functions are registered under their API set and under `kernel32`.

use std::sync::OnceLock;

fn cookie() -> u64 {
    static COOKIE: OnceLock<u64> = OnceLock::new();
    *COOKIE.get_or_init(|| {
        let mut v = 0u64;
        let n = unsafe { libc::getrandom((&mut v as *mut u64).cast(), 8, 0) };
        if n != 8 || v == 0 {
            // No entropy source: still a valid (if guessable) key, from the address space layout.
            v = &v as *const u64 as u64 ^ 0x9E37_79B9_7F4A_7C15;
        }
        v
    })
}

const SYSTEM_COOKIE: u64 = 0x5A3C_96E1_B24D_78F5;

fn encode(p: u64, key: u64) -> u64 {
    (p ^ key).rotate_right((key & 63) as u32)
}

fn decode(p: u64, key: u64) -> u64 {
    p.rotate_left((key & 63) as u32) ^ key
}

win32! {
    module "api-ms-win-core-util" => "kernel32";
    Native fn EncodePointer(p: u64) -> u64 { encode(p, cookie()) }
    Native fn DecodePointer(p: u64) -> u64 { decode(p, cookie()) }
    Native fn EncodeSystemPointer(p: u64) -> u64 { encode(p, SYSTEM_COOKIE) }
    Native fn DecodeSystemPointer(p: u64) -> u64 { decode(p, SYSTEM_COOKIE) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered() {
        for n in [
            "EncodePointer",
            "DecodePointer",
            "EncodeSystemPointer",
            "DecodeSystemPointer",
        ] {
            assert!(crate::lookup("api-ms-win-core-util-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("KERNEL32.dll", n).is_some());
        }
    }

    #[test]
    fn round_trip() {
        for p in [0u64, 1, 0x1234_5678, 0x7FFF_FFFF_FFFF, u64::MAX] {
            unsafe {
                assert_eq!(DecodePointer(EncodePointer(p)), p);
                assert_eq!(DecodeSystemPointer(EncodeSystemPointer(p)), p);
            }
        }
    }

    #[test]
    fn encoding_hides_the_pointer() {
        unsafe {
            assert_ne!(EncodePointer(0), 0);
            assert_ne!(EncodePointer(0x1000), 0x1000);
            assert_ne!(EncodePointer(0x1000), EncodePointer(0x2000));
            assert_eq!(EncodePointer(0x1000), EncodePointer(0x1000));
            // the two keys are independent
            assert_ne!(DecodePointer(EncodeSystemPointer(0x1000)), 0x1000);
        }
    }
}
