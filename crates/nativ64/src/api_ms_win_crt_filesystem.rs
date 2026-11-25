//! `api-ms-win-crt-filesystem`: `_wstat64`, `remove`, `_lock_file`, `_unlock_file`.
//!
//! Paths follow the rule of [`crate::paths`]: `_wstat64` looks under the write directory first,
//! then in the package; `remove` only touches the write directory (the package is never
//! written), like `DeleteFileW`. Failures set `errno` (`ENOENT`, `EACCES`, `EINVAL`).
//!
//! `_wstat64` fills the UCRT's `struct __stat64` (56 bytes): `st_mode` carries the file type
//! (`_S_IFDIR`/`_S_IFREG`) and the owner permission bits, copied to group and others; the device,
//! inode, owner and rdev fields stay zero except `st_nlink` (1). `_lock_file` and `_unlock_file`
//! are no-ops: our `FILE` is unbuffered and its fields are atomic.

use crate::api_ms_win_core_file::{
    delete_file, stat_path, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};
use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use std::ffi::c_void;
use std::os::unix::fs::MetadataExt;

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IREAD: u16 = 0x100;
const S_IWRITE: u16 = 0x80;
const S_IEXEC: u16 = 0x40;
const STAT64_SIZE: usize = 56;

/// Writes `value` at byte `offset` of the `__stat64` buffer.
unsafe fn put<T>(buf: *mut u8, offset: usize, value: T) {
    std::ptr::write_unaligned(buf.add(offset) as *mut T, value);
}

/// The UCRT's `st_mode` for a host file: type, then rwx of the owner copied to group/others.
fn mode_of(m: &std::fs::Metadata) -> u16 {
    let mut bits = S_IREAD;
    if m.mode() & 0o200 != 0 {
        bits |= S_IWRITE;
    }
    if m.is_dir() || m.mode() & 0o100 != 0 {
        bits |= S_IEXEC;
    }
    let kind = if m.is_dir() { S_IFDIR } else { S_IFREG };
    kind | bits | (bits >> 3) | (bits >> 6)
}

unsafe fn fill_stat(buf: *mut u8, m: &std::fs::Metadata) {
    std::ptr::write_bytes(buf, 0, STAT64_SIZE);
    put(buf, 6, mode_of(m));
    put(buf, 8, 1i16);
    put(buf, 24, m.len() as i64);
    put(buf, 32, m.atime());
    put(buf, 40, m.mtime());
    put(buf, 48, m.ctime());
}

unsafe fn stat_w(path: *const u16, buf: *mut u8) -> i32 {
    if path.is_null() || buf.is_null() {
        invalid_parameter_err(EINVAL);
        return -1;
    }
    let Some(p) = crate::handles::name_w(path) else {
        invalid_parameter_err(EINVAL);
        return -1;
    };
    match stat_path(&p) {
        Ok(m) => {
            fill_stat(buf, &m);
            0
        }
        Err(_) => {
            set_errno(ENOENT);
            -1
        }
    }
}

unsafe fn remove_path(path: *const u8) -> i32 {
    if path.is_null() {
        invalid_parameter_err(EINVAL);
        return -1;
    }
    let bytes = std::ffi::CStr::from_ptr(path as *const std::ffi::c_char).to_bytes();
    let p = String::from_utf8_lossy(bytes).into_owned();
    match delete_file(&p) {
        Ok(()) => 0,
        Err(e) => {
            set_errno(if e == ERROR_FILE_NOT_FOUND || e == ERROR_PATH_NOT_FOUND {
                ENOENT
            } else {
                EACCES
            });
            -1
        }
    }
}

crate::win32! {
    module "api-ms-win-crt-filesystem" => "ucrtbase";

    Native fn _wstat64(path: *const u16, buf: *mut u8) -> i32 {
        stat_w(path, buf)
    }
    Native fn remove(path: *const u8) -> i32 {
        remove_path(path)
    }
    Native fn _lock_file(_file: *mut c_void) {}
    Native fn _unlock_file(_file: *mut c_void) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{errno, test_support::*};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn stat_of_dir_file_and_missing() {
        let _g = install_counting_handler();
        unsafe {
            let dir = std::env::temp_dir();
            let mut b = [0xffu8; STAT64_SIZE];
            assert_eq!(
                _wstat64(wide(dir.to_str().unwrap()).as_ptr(), b.as_mut_ptr()),
                0
            );
            let mode = u16::from_le_bytes([b[6], b[7]]);
            assert_eq!(mode & 0xf000, S_IFDIR);
            assert_eq!(mode & 0o777, 0o777);

            let f = dir.join(format!("nativ64-stat-{}", std::process::id()));
            std::fs::write(&f, b"hello").unwrap();
            assert_eq!(
                _wstat64(wide(f.to_str().unwrap()).as_ptr(), b.as_mut_ptr()),
                0
            );
            assert_eq!(u16::from_le_bytes([b[6], b[7]]) & 0xf000, S_IFREG);
            assert_eq!(i64::from_le_bytes(b[24..32].try_into().unwrap()), 5);
            assert!(i64::from_le_bytes(b[40..48].try_into().unwrap()) > 1_000_000_000);
            std::fs::remove_file(&f).unwrap();

            assert_eq!(
                _wstat64(wide(f.to_str().unwrap()).as_ptr(), b.as_mut_ptr()),
                -1
            );
            assert_eq!(errno(), ENOENT);
        }
    }

    #[test]
    fn stat_rejects_null_arguments() {
        let _g = install_counting_handler();
        unsafe {
            let mut b = [0u8; STAT64_SIZE];
            assert_eq!(_wstat64(std::ptr::null(), b.as_mut_ptr()), -1);
            assert_eq!(errno(), EINVAL);
            let w = wide("/tmp");
            assert_eq!(_wstat64(w.as_ptr(), std::ptr::null_mut()), -1);
            assert_eq!(remove(std::ptr::null()), -1);
            assert_eq!(calls(), 3);
        }
    }

    #[test]
    fn remove_missing_file_is_enoent() {
        let _g = install_counting_handler();
        unsafe {
            let p = std::ffi::CString::new(format!("/tmp/nativ64-no-such-{}", std::process::id()))
                .unwrap();
            assert_eq!(remove(p.as_ptr() as *const u8), -1);
            assert_eq!(errno(), ENOENT);
        }
    }

    #[test]
    fn exports_are_registered() {
        for n in ["_wstat64", "remove", "_lock_file", "_unlock_file"] {
            assert!(
                crate::lookup("api-ms-win-crt-filesystem.dll", n).is_some(),
                "{n}"
            );
        }
    }
}
