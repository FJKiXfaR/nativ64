//! `api-ms-win-core-libraryloader`: module handles, `GetProcAddress` and `LoadLibraryEx`.
//!
//! Modules are the main image (registered with [`crate::process::set_main_module`]) and the
//! host modules of this crate (`kernel32`, `api-ms-win-*`, `ucrtbase`, ...). A host module gets
//! a synthetic handle (never dereferenced, distinct per module) and `GetProcAddress` resolves
//! through [`crate::lookup`]. Guest DLLs the base mapped and registered with
//! [`crate::process::register_module`] come first: they have their real base as handle and
//! `GetProcAddress` (by name or ordinal) resolves through their export table. Any other DLL
//! is not found (`ERROR_MOD_NOT_FOUND`); there is no run-time loading of a guest DLL yet.
//!
//! Gaps: no reference counting (`FreeLibrary` of a valid handle always succeeds), no export
//! table for the main image (`GetProcAddress` on it fails), no ordinal lookup, no path search
//! (only the file name of a path is used), `LOAD_LIBRARY_AS_DATAFILE` and friends are ignored.
//! `GetModuleFileName` of a host module returns `C:\Windows\System32\<name>.dll`; of the main
//! module, the path given by the base as is.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::process::{main_module, registered_module, registered_module_at, ExportRef};
use std::ffi::{c_char, c_void, CStr};
use std::sync::Mutex;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_MOD_NOT_FOUND: u32 = 126;
const ERROR_PROC_NOT_FOUND: u32 = 127;

/// Synthetic handles: `HOST_BASE + index * HOST_STEP` (looks like a mapped image, never used).
const HOST_BASE: usize = 0x7ff0_0000_0000;
const HOST_STEP: usize = 0x10_0000;

/// Normalized names of the host modules handed out so far (the index is the handle's).
static HOST_MODULES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The last path component, without a directory.
fn file_name(name: &str) -> &str {
    name.rsplit(['\\', '/']).next().unwrap_or(name)
}

fn is_host_module(normalized: &str) -> bool {
    crate::exports().any(|e| e.module == normalized)
}

/// The handle of the host module `name` (handed out on first use), if the crate has it.
fn host_handle(name: &str) -> Option<usize> {
    let n = crate::normalize_module(file_name(name));
    if !is_host_module(&n) {
        return None;
    }
    let mut v = HOST_MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let i = v.iter().position(|m| *m == n).unwrap_or_else(|| {
        v.push(n);
        v.len() - 1
    });
    Some(HOST_BASE + i * HOST_STEP)
}

fn host_name(h: usize) -> Option<String> {
    let d = h.checked_sub(HOST_BASE)?;
    if d % HOST_STEP != 0 {
        return None;
    }
    HOST_MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(d / HOST_STEP)
        .cloned()
}

/// True when `name` (a file name, extension optional) designates the main module.
fn is_main_name(name: &str) -> Option<usize> {
    let m = main_module()?;
    let want = file_name(name).to_ascii_lowercase();
    let have = file_name(&m.path).to_ascii_lowercase();
    let strip = |s: &str| s.strip_suffix(".exe").unwrap_or(s).to_string();
    (!want.is_empty() && strip(&want) == strip(&have)).then_some(m.base)
}

/// Resolves a module name to a handle: the main module, the registered guest DLLs, then the
/// host modules.
fn find_module(name: &str) -> Option<usize> {
    is_main_name(name)
        .or_else(|| registered_module(file_name(name)).map(|m| m.base))
        .or_else(|| host_handle(name))
}

unsafe fn wide_to_string(p: *const u16) -> String {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

unsafe fn ansi_to_string(p: *const c_char) -> String {
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// The path of module `h` (NULL = main module), or None for an unknown handle.
fn module_path(h: usize) -> Option<String> {
    let main = main_module();
    if h == 0 {
        return main.map(|m| m.path);
    }
    if main.as_ref().is_some_and(|m| m.base == h) {
        return main.map(|m| m.path);
    }
    if let Some(m) = registered_module_at(h) {
        return Some(m.path);
    }
    host_name(h).map(|n| format!("C:\\Windows\\System32\\{n}.dll"))
}

/// Shared by `GetModuleHandle{A,W}`: NULL name is the main module.
fn module_handle(name: Option<String>) -> usize {
    let found = match name {
        None => main_module().map(|m| m.base),
        Some(n) => find_module(&n),
    };
    found.unwrap_or_else(|| {
        set_last_error(ERROR_MOD_NOT_FOUND);
        0
    })
}

/// Copies `s` as UTF-16 into `buf` (capacity `size`), truncating like `GetModuleFileNameW`.
unsafe fn store_w(s: &str, buf: *mut u16, size: u32) -> u32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let cap = size as usize;
    let n = w.len().min(cap);
    std::ptr::copy_nonoverlapping(w.as_ptr(), buf, n);
    if n < cap {
        *buf.add(n) = 0;
        n as u32
    } else {
        *buf.add(cap - 1) = 0;
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        size
    }
}

unsafe fn store_a(s: &str, buf: *mut u8, size: u32) -> u32 {
    let b = s.as_bytes();
    let cap = size as usize;
    let n = b.len().min(cap);
    std::ptr::copy_nonoverlapping(b.as_ptr(), buf, n);
    if n < cap {
        *buf.add(n) = 0;
        n as u32
    } else {
        *buf.add(cap - 1) = 0;
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        size
    }
}

/// `GetModuleFileName*` common part; `store` writes the string and returns the result.
unsafe fn module_file_name(h: usize, buf: *mut c_void, size: u32, wide: bool) -> u32 {
    if size == 0 {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    if buf.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(path) = module_path(h) else {
        set_last_error(if h == 0 {
            ERROR_MOD_NOT_FOUND
        } else {
            ERROR_INVALID_HANDLE
        });
        return 0;
    };
    if wide {
        store_w(&path, buf as *mut u16, size)
    } else {
        store_a(&path, buf as *mut u8, size)
    }
}

unsafe fn proc_address(h: usize, name: *const c_char) -> *const c_void {
    let fail = |e| {
        set_last_error(e);
        std::ptr::null()
    };
    if name.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    if let Some(m) = registered_module_at(h) {
        let r = if (name as usize) < 0x10000 {
            ExportRef::Ordinal(name as usize as u16)
        } else {
            match CStr::from_ptr(name).to_str() {
                Ok(n) => ExportRef::Name(n),
                Err(_) => return fail(ERROR_PROC_NOT_FOUND),
            }
        };
        return match m.export(r) {
            Some(a) => a as *const c_void,
            None => fail(ERROR_PROC_NOT_FOUND),
        };
    }
    let Some(module) = host_name(h) else {
        // The main module has no export table here; anything else is not a module.
        let main = main_module().is_some_and(|m| m.base == h);
        return fail(if main {
            ERROR_PROC_NOT_FOUND
        } else {
            ERROR_INVALID_HANDLE
        });
    };
    // An ordinal is a pointer value below 64 KiB.
    if (name as usize) < 0x10000 {
        return fail(ERROR_PROC_NOT_FOUND);
    }
    let Ok(n) = CStr::from_ptr(name).to_str() else {
        return fail(ERROR_PROC_NOT_FOUND);
    };
    match crate::lookup(&module, n) {
        Some(e) => e.addr as *const c_void,
        None => fail(ERROR_PROC_NOT_FOUND),
    }
}

fn load_library(name: String) -> usize {
    find_module(&name).unwrap_or_else(|| {
        set_last_error(ERROR_MOD_NOT_FOUND);
        0
    })
}

win32! {
    module "api-ms-win-core-libraryloader" => "kernel32";
    Native fn GetModuleHandleW(name: *const u16) -> usize {
        module_handle((!name.is_null()).then(|| wide_to_string(name)))
    }
    Native fn GetModuleHandleA(name: *const c_char) -> usize {
        module_handle((!name.is_null()).then(|| ansi_to_string(name)))
    }
    Native fn GetModuleFileNameW(h: usize, buf: *mut u16, size: u32) -> u32 {
        module_file_name(h, buf as *mut c_void, size, true)
    }
    Native fn GetModuleFileNameA(h: usize, buf: *mut u8, size: u32) -> u32 {
        module_file_name(h, buf as *mut c_void, size, false)
    }
    Native fn GetProcAddress(h: usize, name: *const c_char) -> *const c_void {
        proc_address(h, name)
    }
    Native fn LoadLibraryExW(name: *const u16, _file: usize, _flags: u32) -> usize {
        if name.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        load_library(wide_to_string(name))
    }
    Native fn LoadLibraryW(name: *const u16) -> usize {
        if name.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        load_library(wide_to_string(name))
    }
    Native fn LoadLibraryA(name: *const c_char) -> usize {
        if name.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        load_library(ansi_to_string(name))
    }
    Native fn LoadLibraryExA(name: *const c_char, _file: usize, _flags: u32) -> usize {
        if name.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        load_library(ansi_to_string(name))
    }
    Native fn FreeLibrary(h: usize) -> i32 {
        let known = main_module().is_some_and(|m| m.base == h)
            || registered_module_at(h).is_some()
            || host_name(h).is_some();
        if !known {
            set_last_error(ERROR_INVALID_HANDLE);
        }
        known as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn registered_both_names() {
        for n in [
            "GetModuleHandleW",
            "GetProcAddress",
            "LoadLibraryExW",
            "LoadLibraryW",
            "LoadLibraryA",
            "FreeLibrary",
        ] {
            assert!(crate::lookup("api-ms-win-core-libraryloader-l1-2-0.dll", n).is_some());
            assert!(crate::lookup("kernel32", n).is_some());
        }
    }

    #[test]
    fn host_module_handles_and_proc_address() {
        unsafe {
            let k = LoadLibraryExW(w("KERNEL32.dll").as_ptr(), 0, 0);
            assert_ne!(k, 0);
            assert_eq!(GetModuleHandleW(w("kernel32").as_ptr()), k);
            assert_eq!(GetModuleHandleA(c"Kernel32.DLL".as_ptr()), k);
            assert_eq!(
                LoadLibraryExA(c"C:\\Windows\\System32\\kernel32.dll".as_ptr(), 0, 0),
                k
            );
            let a = LoadLibraryExW(w("api-ms-win-core-synch-l1-2-0.dll").as_ptr(), 0, 0);
            assert!(a != 0 && a != k);

            let p = GetProcAddress(k, c"GetTickCount64".as_ptr());
            assert_eq!(
                p,
                crate::api_ms_win_core_sysinfo::GetTickCount64 as *const c_void
            );
            assert!(GetProcAddress(k, c"NoSuchFunction".as_ptr()).is_null());
            assert_eq!(GetLastError(), ERROR_PROC_NOT_FOUND);
            assert!(GetProcAddress(k, 7 as *const c_char).is_null());
            assert_eq!(GetLastError(), ERROR_PROC_NOT_FOUND);
            assert!(GetProcAddress(k, std::ptr::null()).is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert!(GetProcAddress(0x1234, c"GetTickCount64".as_ptr()).is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);

            assert_eq!(LoadLibraryW(w("kernel32.dll").as_ptr()), k);
            assert_eq!(LoadLibraryA(c"kernel32.dll".as_ptr()), k);
            assert_ne!(LoadLibraryW(w("xgameruntime.dll").as_ptr()), 0);
            assert_eq!(LoadLibraryW(w("nosuch.dll").as_ptr()), 0);
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            assert_eq!(LoadLibraryW(std::ptr::null()), 0);

            assert_eq!(FreeLibrary(k), 1);
            assert_eq!(FreeLibrary(0x1234), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        }
    }

    #[test]
    fn unknown_dll_is_not_found() {
        unsafe {
            assert_eq!(LoadLibraryExW(w("nosuch.dll").as_ptr(), 0, 0), 0);
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            assert_eq!(GetModuleHandleW(w("nosuch.dll").as_ptr()), 0);
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            assert_eq!(LoadLibraryExW(std::ptr::null(), 0, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn module_file_names() {
        unsafe {
            let k = LoadLibraryExW(w("kernel32.dll").as_ptr(), 0, 0);
            let mut buf = [0u16; 128];
            let n = GetModuleFileNameW(k, buf.as_mut_ptr(), 128) as usize;
            assert_eq!(
                String::from_utf16_lossy(&buf[..n]),
                "C:\\Windows\\System32\\kernel32.dll"
            );
            let mut a = [0u8; 128];
            assert_eq!(GetModuleFileNameA(k, a.as_mut_ptr(), 128) as usize, n);
            // Truncation: size returned, NUL-terminated, ERROR_INSUFFICIENT_BUFFER.
            let mut small = [0xffffu16; 8];
            assert_eq!(GetModuleFileNameW(k, small.as_mut_ptr(), 8), 8);
            assert_eq!(small[7], 0);
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            assert_eq!(GetModuleFileNameW(k, buf.as_mut_ptr(), 0), 0);
            assert_eq!(GetModuleFileNameW(0x1234, buf.as_mut_ptr(), 128), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            assert_eq!(GetModuleFileNameW(k, std::ptr::null_mut(), 128), 0);
        }
    }

    #[test]
    fn main_module() {
        crate::process::set_main_module(0x1_4000_0000, "/games/prog.exe");
        unsafe {
            assert_eq!(GetModuleHandleW(std::ptr::null()), 0x1_4000_0000);
            assert_eq!(GetModuleHandleW(w("PROG.exe").as_ptr()), 0x1_4000_0000);
            assert_eq!(GetModuleHandleA(c"prog".as_ptr()), 0x1_4000_0000);
            let mut buf = [0u16; 64];
            let n = GetModuleFileNameW(0, buf.as_mut_ptr(), 64) as usize;
            assert_eq!(String::from_utf16_lossy(&buf[..n]), "/games/prog.exe");
            let n = GetModuleFileNameW(0x1_4000_0000, buf.as_mut_ptr(), 64) as usize;
            assert_eq!(n, 15);
            assert!(GetProcAddress(0x1_4000_0000, c"Foo".as_ptr()).is_null());
            assert_eq!(GetLastError(), ERROR_PROC_NOT_FOUND);
            assert_eq!(FreeLibrary(0x1_4000_0000), 1);
        }
    }
}
