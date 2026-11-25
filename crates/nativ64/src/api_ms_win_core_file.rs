//! File APIs of `kernel32` (`api-ms-win-core-file`), on the path rule of [`crate::paths`].
//! Done so far: `GetFileAttributesW/A` (directory, read-only or normal), `CreateFileW`,
//! `ReadFile`, `WriteFile`, `SetFilePointerEx`, `GetFileSizeEx`, `FlushFileBuffers`.
//!
//! Reads look under the write directory first (what the title saved), then in the package;
//! anything that may modify a file lives only under the write directory (`paths`). Overlapped
//! I/O is done synchronously at the offset in the `OVERLAPPED`. Sharing and flags are ignored.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::paths::map_read_path;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_FILE_EXISTS: u32 = 80;
const ERROR_ALREADY_EXISTS: u32 = 183;
const INVALID_HANDLE_VALUE: usize = usize::MAX;
const WRITE_ACCESS: u32 = 0x4000_0000 | 0x1000_0000 | 0x2 | 0x4;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;

fn last_error_of(e: i32) -> u32 {
    match e {
        libc::ENOTDIR => ERROR_PATH_NOT_FOUND,
        libc::EACCES | libc::EPERM | libc::EROFS | libc::EISDIR => ERROR_ACCESS_DENIED,
        libc::EEXIST => ERROR_FILE_EXISTS,
        libc::EBADF => ERROR_INVALID_HANDLE,
        _ => ERROR_FILE_NOT_FOUND,
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn open_path(path: &str, access: u32, disposition: u32) -> Result<(OwnedFd, bool), u32> {
    let write = access & WRITE_ACCESS != 0
        || matches!(
            disposition,
            CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING | OPEN_ALWAYS
        );
    let (host, flags) = if write {
        let Some(h) = crate::paths::map_write_path(path) else {
            return Err(ERROR_FILE_NOT_FOUND);
        };
        if let Some(parent) = h.parent() {
            if disposition != OPEN_EXISTING && disposition != TRUNCATE_EXISTING {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        // One descriptor serves reads and writes.
        let mut f = libc::O_RDWR;
        f |= match disposition {
            CREATE_NEW => libc::O_CREAT | libc::O_EXCL,
            CREATE_ALWAYS => libc::O_CREAT | libc::O_TRUNC,
            OPEN_ALWAYS => libc::O_CREAT,
            TRUNCATE_EXISTING => libc::O_TRUNC,
            _ => 0,
        };
        (h, f)
    } else {
        let saved = crate::paths::map_write_path(path).filter(|p| p.exists());
        (saved.unwrap_or_else(|| map_read_path(path)), libc::O_RDONLY)
    };
    let existed = host.exists();
    let c = std::ffi::CString::new(host.as_os_str().as_encoded_bytes())
        .map_err(|_| ERROR_FILE_NOT_FOUND)?;
    // SAFETY: a valid NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o644) };
    if fd < 0 {
        return Err(last_error_of(errno()));
    }
    // SAFETY: a descriptor we just opened.
    Ok((unsafe { OwnedFd::from_raw_fd(fd) }, existed))
}

/// The offset of an `OVERLAPPED` (`Offset`, `OffsetHigh` at bytes 16 and 20).
unsafe fn overlapped_offset(o: *const c_void) -> u64 {
    let p = o as *const u32;
    (*p.add(5) as u64) << 32 | *p.add(4) as u64
}

unsafe fn transfer(
    h: usize,
    buf: *mut u8,
    n: u32,
    done: *mut u32,
    ov: *const c_void,
    write: bool,
) -> i32 {
    let r = crate::handles::with_file(h, |f| {
        let fd = f.fd.as_raw_fd();
        let at = if ov.is_null() {
            *f.pos.lock().unwrap_or_else(|e| e.into_inner())
        } else {
            overlapped_offset(ov)
        };
        let got = loop {
            let r = if write {
                libc::pwrite(fd, buf as *const c_void, n as usize, at as libc::off_t)
            } else {
                libc::pread(fd, buf as *mut c_void, n as usize, at as libc::off_t)
            };
            if r < 0 && errno() == libc::EINTR {
                continue;
            }
            break r;
        };
        if got >= 0 && ov.is_null() {
            *f.pos.lock().unwrap_or_else(|e| e.into_inner()) = at + got as u64;
        }
        (got, errno())
    });
    match r {
        Some((got, _)) if got >= 0 => {
            if !done.is_null() {
                *done = got as u32;
            }
            1
        }
        Some((_, e)) => {
            set_last_error(last_error_of(e));
            0
        }
        None => 0,
    }
}

fn attributes(path: &str) -> u32 {
    match std::fs::metadata(map_read_path(path)) {
        Ok(m) => {
            let mut a = if m.is_dir() {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            if m.mode() & 0o222 == 0 {
                a = (a & !FILE_ATTRIBUTE_NORMAL) | FILE_ATTRIBUTE_READONLY;
            }
            a
        }
        Err(e) => {
            set_last_error(match e.raw_os_error() {
                Some(libc::ENOTDIR) => ERROR_PATH_NOT_FOUND,
                Some(libc::EACCES) => ERROR_ACCESS_DENIED,
                _ => ERROR_FILE_NOT_FOUND,
            });
            INVALID_FILE_ATTRIBUTES
        }
    }
}

crate::win32! {
    module "api-ms-win-core-file" => "kernel32";

    Native fn GetFileAttributesW(path: *const u16) -> u32 {
        match crate::handles::name_w(path) {
            Some(p) => attributes(&p),
            None => {
                set_last_error(crate::handles::ERROR_INVALID_PARAMETER);
                INVALID_FILE_ATTRIBUTES
            }
        }
    }
    Native fn CreateFileW(path: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: usize) -> usize {
        let _ = (share, security, flags, template);
        let Some(p) = crate::handles::name_w(path) else {
            set_last_error(crate::handles::ERROR_INVALID_PARAMETER);
            return INVALID_HANDLE_VALUE;
        };
        match open_path(&p, access, disposition) {
            Ok((fd, existed)) => {
                set_last_error(if existed && matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS) {
                    ERROR_ALREADY_EXISTS
                } else {
                    0
                });
                crate::handles::create_file_handle(fd)
            }
            Err(e) => {
                set_last_error(e);
                INVALID_HANDLE_VALUE
            }
        }
    }
    Native fn ReadFile(h: usize, buf: *mut u8, n: u32, read: *mut u32, overlapped: *const c_void) -> i32 {
        transfer(h, buf, n, read, overlapped, false)
    }
    Native fn WriteFile(h: usize, buf: *const u8, n: u32, written: *mut u32, overlapped: *const c_void) -> i32 {
        transfer(h, buf as *mut u8, n, written, overlapped, true)
    }
    // method: 0 begin, 1 current, 2 end.
    Native fn SetFilePointerEx(h: usize, distance: i64, new_pos: *mut i64, method: u32) -> i32 {
        let r = crate::handles::with_file(h, |f| {
            let mut pos = f.pos.lock().unwrap_or_else(|e| e.into_inner());
            let base = match method {
                0 => 0,
                1 => *pos as i64,
                2 => {
                    let mut st: libc::stat = std::mem::zeroed();
                    libc::fstat(f.fd.as_raw_fd(), &mut st);
                    st.st_size
                }
                _ => return None,
            };
            let target = base.checked_add(distance).filter(|t| *t >= 0)?;
            *pos = target as u64;
            Some(target)
        });
        match r {
            Some(Some(t)) => {
                if !new_pos.is_null() {
                    *new_pos = t;
                }
                1
            }
            Some(None) => {
                set_last_error(crate::handles::ERROR_INVALID_PARAMETER);
                0
            }
            None => 0,
        }
    }
    Native fn GetFileSizeEx(h: usize, size: *mut i64) -> i32 {
        let r = crate::handles::with_file(h, |f| {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(f.fd.as_raw_fd(), &mut st) == 0).then_some(st.st_size)
        });
        match r {
            Some(Some(s)) if !size.is_null() => {
                *size = s;
                1
            }
            Some(_) => {
                set_last_error(crate::handles::ERROR_INVALID_PARAMETER);
                0
            }
            None => 0,
        }
    }
    // Creation, access and write times as FILETIMEs (100 ns since 1601); creation uses the
    // status-change time. Any of the three pointers may be NULL.
    Native fn GetFileTime(h: usize, created: *mut u64, accessed: *mut u64, written: *mut u64) -> i32 {
        const EPOCH_DIFF: u64 = 11_644_473_600;
        let ft = |sec: i64, nsec: i64| (sec as u64 + EPOCH_DIFF) * 10_000_000 + nsec as u64 / 100;
        let r = crate::handles::with_file(h, |f| {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(f.fd.as_raw_fd(), &mut st) == 0).then_some(st)
        });
        match r {
            Some(Some(st)) => {
                for (p, v) in [
                    (created, ft(st.st_ctime, st.st_ctime_nsec)),
                    (accessed, ft(st.st_atime, st.st_atime_nsec)),
                    (written, ft(st.st_mtime, st.st_mtime_nsec)),
                ] {
                    if !p.is_null() {
                        p.write_unaligned(v);
                    }
                }
                1
            }
            Some(None) => {
                set_last_error(ERROR_INVALID_HANDLE);
                0
            }
            None => 0,
        }
    }
    Native fn FlushFileBuffers(h: usize) -> i32 {
        crate::handles::with_file(h, |f| {
            libc::fsync(f.fd.as_raw_fd());
            1
        })
        .unwrap_or(0)
    }
    Native fn GetFileAttributesA(path: *const u8) -> u32 {
        match crate::handles::name_a(path) {
            Some(p) => attributes(&p),
            None => {
                set_last_error(crate::handles::ERROR_INVALID_PARAMETER);
                INVALID_FILE_ATTRIBUTES
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_of_dir_file_and_missing() {
        unsafe {
            let dir = std::env::temp_dir();
            let w: Vec<u16> = dir.to_str().unwrap().encode_utf16().chain([0]).collect();
            assert_eq!(GetFileAttributesW(w.as_ptr()), FILE_ATTRIBUTE_DIRECTORY);
            let f = dir.join(format!("nativ64-attr-{}", std::process::id()));
            std::fs::write(&f, b"x").unwrap();
            let c = std::ffi::CString::new(f.to_str().unwrap()).unwrap();
            assert_eq!(
                GetFileAttributesA(c.as_ptr() as *const u8),
                FILE_ATTRIBUTE_NORMAL
            );
            std::fs::remove_file(&f).unwrap();
            assert_eq!(
                GetFileAttributesA(c.as_ptr() as *const u8),
                INVALID_FILE_ATTRIBUTES
            );
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_FILE_NOT_FOUND
            );
            assert!(crate::lookup("kernel32", "GetFileAttributesW").is_some());
        }
    }

    #[test]
    fn open_read_seek_and_missing() {
        unsafe {
            let f = std::env::temp_dir().join(format!("nativ64-cf-{}", std::process::id()));
            std::fs::write(&f, b"0123456789").unwrap();
            let w: Vec<u16> = f.to_str().unwrap().encode_utf16().chain([0]).collect();
            let h = CreateFileW(
                w.as_ptr(),
                0x8000_0000,
                1,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            assert_ne!(h, INVALID_HANDLE_VALUE);
            let mut size = 0i64;
            assert_eq!(GetFileSizeEx(h, &mut size), 1);
            assert_eq!(size, 10);
            let mut pos = 0i64;
            assert_eq!(SetFilePointerEx(h, -4, &mut pos, 2), 1);
            assert_eq!(pos, 6);
            let mut buf = [0u8; 8];
            let mut n = 0u32;
            assert_eq!(
                ReadFile(h, buf.as_mut_ptr(), 8, &mut n, std::ptr::null()),
                1
            );
            assert_eq!((n, &buf[..4]), (4, &b"6789"[..]));
            assert_eq!(
                ReadFile(h, buf.as_mut_ptr(), 8, &mut n, std::ptr::null()),
                1
            );
            assert_eq!(n, 0);
            assert_eq!(SetFilePointerEx(h, -1, &mut pos, 0), 0);
            assert_eq!(crate::handles::close_handle(h), 1);
            assert_eq!(
                ReadFile(h, buf.as_mut_ptr(), 1, &mut n, std::ptr::null()),
                0
            );
            std::fs::remove_file(&f).unwrap();
            let h = CreateFileW(
                w.as_ptr(),
                0x8000_0000,
                1,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            );
            assert_eq!(h, INVALID_HANDLE_VALUE);
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_FILE_NOT_FOUND
            );
        }
    }
}
