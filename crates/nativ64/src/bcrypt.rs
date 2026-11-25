//! `bcrypt`: the CNG entry points the title imports (algorithm providers and random bytes).
//!
//! The title only opens a provider, asks it for random bytes and closes it. Providers are
//! opaque handles into a small table; only the algorithm name is kept. Hash and cipher
//! providers can be opened but have no operations yet (their functions are not imported).

use std::collections::BTreeMap;
use std::sync::Mutex;

pub type NtStatus = i32;
pub const STATUS_SUCCESS: NtStatus = 0;
pub const STATUS_INVALID_PARAMETER: NtStatus = 0xC000_000Du32 as i32;
pub const STATUS_INVALID_HANDLE: NtStatus = 0xC000_0008u32 as i32;
pub const STATUS_NOT_FOUND: NtStatus = 0xC000_0225u32 as i32;

const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;

/// Algorithm identifiers CNG knows (case-insensitive, like Windows).
const KNOWN: &[&str] = &[
    "RNG",
    "MD5",
    "SHA1",
    "SHA256",
    "SHA384",
    "SHA512",
    "AES",
    "3DES",
    "RSA",
    "ECDSA_P256",
    "ECDH_P256",
    "ECDSA_P384",
    "ECDH_P384",
    "DSA",
    "DH",
    "HMAC",
];

struct Provider {
    algorithm: String,
}

struct Table {
    next: usize,
    map: BTreeMap<usize, Provider>,
}

static PROVIDERS: Mutex<Table> = Mutex::new(Table {
    next: 0x1000,
    map: BTreeMap::new(),
});

unsafe fn read_wstr(p: *const u16) -> String {
    let mut v = Vec::new();
    let mut i = 0;
    while *p.add(i) != 0 && i < 256 {
        v.push(*p.add(i));
        i += 1;
    }
    String::from_utf16_lossy(&v)
}

fn fill_random(buf: &mut [u8]) -> bool {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe { libc::getrandom(buf[done..].as_mut_ptr().cast(), buf.len() - done, 0) };
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    true
}

win32! {
    module "bcrypt" => "bcrypt";
    Native fn BCryptOpenAlgorithmProvider(
        handle: *mut usize,
        algorithm: *const u16,
        _implementation: *const u16,
        _flags: u32
    ) -> NtStatus {
        if handle.is_null() || algorithm.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let name = read_wstr(algorithm);
        if !KNOWN.iter().any(|k| k.eq_ignore_ascii_case(&name)) {
            eprintln!("bcrypt: unknown algorithm {name:?}");
            return STATUS_NOT_FOUND;
        }
        let mut t = PROVIDERS.lock().unwrap();
        let h = t.next;
        t.next += 0x10;
        t.map.insert(h, Provider { algorithm: name.to_ascii_uppercase() });
        *handle = h;
        STATUS_SUCCESS
    }
    Native fn BCryptCloseAlgorithmProvider(handle: usize, _flags: u32) -> NtStatus {
        match PROVIDERS.lock().unwrap().map.remove(&handle) {
            Some(_) => STATUS_SUCCESS,
            None => STATUS_INVALID_HANDLE,
        }
    }
    Native fn BCryptGenRandom(
        handle: usize,
        buffer: *mut u8,
        count: u32,
        flags: u32
    ) -> NtStatus {
        if buffer.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        if handle == 0 {
            // A NULL provider needs the system-preferred RNG flag.
            if flags & BCRYPT_USE_SYSTEM_PREFERRED_RNG == 0 {
                return STATUS_INVALID_HANDLE;
            }
        } else {
            let t = PROVIDERS.lock().unwrap();
            match t.map.get(&handle) {
                None => return STATUS_INVALID_HANDLE,
                Some(p) if p.algorithm != "RNG" => return STATUS_INVALID_HANDLE,
                _ => {}
            }
        }
        let slice = std::slice::from_raw_parts_mut(buffer, count as usize);
        if fill_random(slice) {
            STATUS_SUCCESS
        } else {
            STATUS_INVALID_PARAMETER
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{null, null_mut};

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn rng_provider_roundtrip() {
        unsafe {
            let mut h = 0usize;
            let name = w("rng");
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, name.as_ptr(), null(), 0),
                0
            );
            assert_ne!(h, 0);
            let mut a = [0u8; 32];
            let mut b = [0u8; 32];
            assert_eq!(BCryptGenRandom(h, a.as_mut_ptr(), 32, 0), 0);
            assert_eq!(BCryptGenRandom(h, b.as_mut_ptr(), 32, 0), 0);
            assert_ne!(a, [0u8; 32]);
            assert_ne!(a, b);
            assert_eq!(BCryptCloseAlgorithmProvider(h, 0), 0);
            assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_INVALID_HANDLE);
            assert_eq!(
                BCryptGenRandom(h, a.as_mut_ptr(), 4, 0),
                STATUS_INVALID_HANDLE
            );
        }
    }

    #[test]
    fn null_handle_needs_system_flag() {
        unsafe {
            let mut a = [0u8; 16];
            assert_eq!(
                BCryptGenRandom(0, a.as_mut_ptr(), 16, 0),
                STATUS_INVALID_HANDLE
            );
            assert_eq!(BCryptGenRandom(0, a.as_mut_ptr(), 16, 2), 0);
            assert_eq!(
                BCryptGenRandom(0, null_mut(), 16, 2),
                STATUS_INVALID_PARAMETER
            );
        }
    }

    #[test]
    fn open_errors() {
        unsafe {
            let mut h = 0usize;
            let bad = w("NOPE");
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, bad.as_ptr(), null(), 0),
                STATUS_NOT_FOUND
            );
            assert_eq!(
                BCryptOpenAlgorithmProvider(null_mut(), bad.as_ptr(), null(), 0),
                STATUS_INVALID_PARAMETER
            );
            let sha = w("SHA256");
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, sha.as_ptr(), null(), 0),
                0
            );
            let mut a = [0u8; 4];
            assert_eq!(
                BCryptGenRandom(h, a.as_mut_ptr(), 4, 0),
                STATUS_INVALID_HANDLE
            );
            assert_eq!(BCryptCloseAlgorithmProvider(h, 0), 0);
        }
    }
}
