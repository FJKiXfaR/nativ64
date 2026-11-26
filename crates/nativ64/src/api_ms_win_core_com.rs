//! `api-ms-win-core-com` (forwarded to `combase`): the COM runtime entry points.
//!
//! There is no class registry: `CoCreateInstance` knows `MMDeviceEnumerator` (`mmdevapi.rs`) and
//! reports any other class as unregistered.

use crate::com::{Guid, E_INVALIDARG, HRESULT, S_FALSE, S_OK};
use std::cell::Cell;
use std::sync::Mutex;

pub const REGDB_E_CLASSNOTREG: HRESULT = 0x8004_0154u32 as i32;

thread_local! {
    /// Nesting count of `CoInitializeEx` on this thread.
    static INIT_COUNT: Cell<u32> = const { Cell::new(0) };
}

/// CLSIDs already reported, so each missing class is logged once.
static REPORTED: Mutex<Vec<Guid>> = Mutex::new(Vec::new());

fn fmt_guid(g: &Guid) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

fn random_guid() -> Guid {
    let mut b = [0u8; 16];
    let mut done = 0;
    while done < 16 {
        let n = unsafe { libc::getrandom(b[done..].as_mut_ptr().cast(), 16 - done, 0) };
        if n <= 0 {
            break;
        }
        done += n as usize;
    }
    if done < 16 {
        // Fallback: mix the clock and a stack address; unique enough for a GUID.
        let t = crate::api_ms_win_core_profile::monotonic_ns();
        let a = &b as *const _ as u64;
        b[..8].copy_from_slice(&t.to_le_bytes());
        b[8..].copy_from_slice(&a.to_le_bytes());
    }
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    Guid {
        data1: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        data2: u16::from_be_bytes([b[4], b[5]]),
        data3: u16::from_be_bytes([b[6], b[7]]),
        data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    }
}

win32! {
    module "api-ms-win-core-com" => "combase";
    Native fn CoInitializeEx(_reserved: *mut u8, _flags: u32) -> HRESULT {
        INIT_COUNT.with(|c| {
            let n = c.get();
            c.set(n + 1);
            if n == 0 { S_OK } else { S_FALSE }
        })
    }
    Native fn CoUninitialize() {
        INIT_COUNT.with(|c| c.set(c.get().saturating_sub(1)));
    }
    Native fn CoCreateGuid(out: *mut Guid) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        *out = random_guid();
        S_OK
    }
    Native fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut u8,
        _context: u32,
        iid: *const Guid,
        out: *mut *mut u8
    ) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        *out = std::ptr::null_mut();
        if !clsid.is_null() {
            let c = *clsid;
            if c == crate::mmdevapi::CLSID_MMDEVICE_ENUMERATOR {
                return crate::mmdevapi::create_enumerator(outer, iid, out.cast());
            }
            let mut seen = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.contains(&c) {
                seen.push(c);
                eprintln!("nativ64: CoCreateInstance {}: no such class", fmt_guid(&c));
            }
        }
        REGDB_E_CLASSNOTREG
    }
    Native fn CoTaskMemAlloc(size: usize) -> *mut u8 {
        libc::malloc(size.max(1)).cast()
    }
    Native fn CoTaskMemFree(p: *mut u8) {
        libc::free(p.cast());
    }
    Native fn PropVariantClear(pv: *mut u8) -> HRESULT {
        if pv.is_null() {
            return E_INVALIDARG;
        }
        // Shortcut: the contents (strings, arrays, interfaces) are not released.
        std::ptr::write_bytes(pv, 0, 24);
        S_OK
    }
    Native fn StringFromGUID2(guid: *const Guid, buf: *mut u16, max: i32) -> i32 {
        if guid.is_null() || buf.is_null() || max < 39 {
            return 0;
        }
        for (i, c) in fmt_guid(&*guid).bytes().enumerate() {
            *buf.add(i) = c as u16;
        }
        *buf.add(38) = 0;
        39
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{null, null_mut};

    #[test]
    fn init_counts_per_thread() {
        unsafe {
            assert_eq!(CoInitializeEx(null_mut(), 2), S_OK);
            assert_eq!(CoInitializeEx(null_mut(), 0), S_FALSE);
            CoUninitialize();
            CoUninitialize();
            assert_eq!(CoInitializeEx(null_mut(), 0), S_OK);
            CoUninitialize();
            CoUninitialize(); // extra call: no underflow
            assert_eq!(CoInitializeEx(null_mut(), 0), S_OK);
            std::thread::spawn(|| assert_eq!(CoInitializeEx(null_mut(), 0), S_OK))
                .join()
                .unwrap();
        }
    }

    #[test]
    fn guid_is_v4_and_unique() {
        unsafe {
            let (mut a, mut b) = (Guid::new(0, 0, 0, [0; 8]), Guid::new(0, 0, 0, [0; 8]));
            assert_eq!(CoCreateGuid(&mut a), S_OK);
            assert_eq!(CoCreateGuid(&mut b), S_OK);
            assert_ne!(a, b);
            assert_eq!(a.data3 >> 12, 4);
            assert_eq!(a.data4[0] >> 6, 2);
            assert_eq!(CoCreateGuid(null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn create_instance_fails_cleanly() {
        unsafe {
            let clsid = Guid::new(0x1234_5678, 0x9ABC, 0xDEF0, [1, 2, 3, 4, 5, 6, 7, 8]);
            let mut out = std::ptr::dangling_mut::<u8>();
            assert_eq!(
                CoCreateInstance(&clsid, null_mut(), 1, null(), &mut out),
                REGDB_E_CLASSNOTREG
            );
            assert!(out.is_null());
            assert_eq!(
                CoCreateInstance(&clsid, null_mut(), 1, null(), null_mut()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn task_mem_and_propvariant() {
        unsafe {
            CoTaskMemFree(null_mut());
            let p = CoTaskMemAlloc(0);
            assert!(!p.is_null());
            CoTaskMemFree(p);
            let mut pv = [0xAAu8; 24];
            assert_eq!(PropVariantClear(pv.as_mut_ptr()), S_OK);
            assert_eq!(pv, [0u8; 24]);
            assert_eq!(PropVariantClear(null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn string_from_guid() {
        unsafe {
            let g = Guid::new(0x1234_ABCD, 0x00EF, 0x0123, [0xC0, 1, 2, 3, 4, 5, 6, 0x4F]);
            let mut buf = [0xFFFFu16; 40];
            assert_eq!(StringFromGUID2(&g, buf.as_mut_ptr(), 40), 39);
            let s = String::from_utf16(&buf[..38]).unwrap();
            assert_eq!(s, "{1234ABCD-00EF-0123-C001-02030405064F}");
            assert_eq!(buf[38], 0);
            assert_eq!(buf[39], 0xFFFF);
            assert_eq!(StringFromGUID2(&g, buf.as_mut_ptr(), 38), 0);
            assert_eq!(StringFromGUID2(null(), buf.as_mut_ptr(), 40), 0);
        }
    }
}
