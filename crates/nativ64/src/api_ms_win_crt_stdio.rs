//! `api-ms-win-crt-stdio`: the default file translation mode (`_fmode`), the commit mode
//! (`_commode`), the `__stdio_common_v*printf` / `v*scanf` family and a minimal `FILE`.
//!
//! `FILE`: a small struct over a Linux file descriptor, unbuffered (every write is a `write(2)`,
//! so `fflush` has nothing to do), binary (no `\r\n` translation; `_fmode` is not consulted).
//! `__acrt_iob_func(0..=2)` returns stdin, stdout, stderr (fd 0, 1, 2). `fopen`, `_fsopen`,
//! `_wfopen` and `_wfsopen` accept the modes `r w a` with `+`, `b`, `t` (other letters are
//! ignored; the sharing parameter of the `_fsopen` pair is advisory here and ignored);
//! `fclose` of a standard stream only flags it closed. `_get_stream_buffer_pointers` reports an
//! empty buffer.
//!
//! `fopen` / `_wfopen` resolve their path like the Windows file APIs do (`docs/library.md`,
//! File paths): a path written with `/` is a host path; a title path is written under the
//! title's write directory (a `t:` or `C:` prefix dropped, missing parent directories created)
//! and read from an earlier write first, then from the package directories.
//!
//! Printf: see `printf.rs` for the format and its deviations. Return conventions of the
//! `__stdio_common_*` functions follow the UCRT: the character count, or -1 for an invalid
//! format (the invalid parameter handler runs, `errno` = `EINVAL`) or, in the legacy
//! (non-standard) modes, a truncated result. `_s` variants also fail with `ERANGE` when the
//! result does not fit (buffer set to an empty string).
//!
//! `_fmode` is a process-wide `int` (0 until set: text mode). `__p__fmode` returns its address
//! so a caller can read and write it directly.
//!
//! Functions are registered under their API set and under `ucrtbase`, which forwards to them.

use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use crate::printf::{self, read_units, Unit, VaList};
use crate::scanf;
use std::ffi::{c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

const EINVAL: i32 = 22;
const ERANGE: i32 = 34;
const EOF: i32 = -1;
const O_TEXT: i32 = 0x4000;
const O_BINARY: i32 = 0x8000;

static FMODE: AtomicI32 = AtomicI32::new(0);
/// `_commode`: the default commit mode of streams (0 = no commit-to-disk on flush).
static COMMODE: AtomicI32 = AtomicI32::new(0);

/// The `FILE` the C code points at: opaque to it (the UCRT's `FILE` is one placeholder pointer).
pub struct File {
    fd: AtomicI32,
    eof: AtomicBool,
    err: AtomicBool,
    /// Pushed-back byte (`ungetc`), or -1.
    unget: AtomicI32,
}

impl File {
    const fn new(fd: i32) -> Self {
        File {
            fd: AtomicI32::new(fd),
            eof: AtomicBool::new(false),
            err: AtomicBool::new(false),
            unget: AtomicI32::new(-1),
        }
    }
    fn fd(&self) -> i32 {
        self.fd.load(Ordering::Relaxed)
    }
}

static STDIN: File = File::new(0);
static STDOUT: File = File::new(1);
static STDERR: File = File::new(2);
static NO_BUFFER: AtomicUsize = AtomicUsize::new(0);
static NO_COUNT: AtomicI32 = AtomicI32::new(0);

fn is_std(f: *const File) -> bool {
    std::ptr::eq(f, &STDIN) || std::ptr::eq(f, &STDOUT) || std::ptr::eq(f, &STDERR)
}

fn invalid() -> i32 {
    invalid_parameter_err(EINVAL);
    -1
}

fn os_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(EINVAL)
}

fn write_all(fd: i32, mut b: &[u8]) -> bool {
    while !b.is_empty() {
        let n = unsafe { libc::write(fd, b.as_ptr() as *const c_void, b.len()) };
        if n < 0 {
            if os_errno() == libc::EINTR {
                continue;
            }
            return false;
        }
        b = &b[n as usize..];
    }
    true
}

/// Reads up to `b.len()` bytes; fewer only at end of file or on error (`Err`).
fn read_full(fd: i32, b: &mut [u8]) -> Result<usize, ()> {
    let mut got = 0;
    while got < b.len() {
        let n = unsafe { libc::read(fd, b[got..].as_mut_ptr() as *mut c_void, b.len() - got) };
        if n < 0 {
            if os_errno() == libc::EINTR {
                continue;
            }
            return if got > 0 { Ok(got) } else { Err(()) };
        }
        if n == 0 {
            break;
        }
        got += n as usize;
    }
    Ok(got)
}

unsafe fn read_format<T: Unit>(fmt: *const T) -> Vec<u32> {
    read_units(fmt, usize::MAX)
}

unsafe fn put<T: Unit>(buf: *mut T, units: &[u32]) {
    for (k, &u) in units.iter().enumerate() {
        *buf.add(k) = T::from_u32(u);
    }
}

/// Formats; on an invalid format raises the invalid parameter handler.
unsafe fn render<T: Unit>(opts: u64, fmt: *const T, args: *const u8) -> Option<Vec<u32>> {
    let f = read_format(fmt);
    let mut va = VaList::new(args);
    match printf::format(&f, T::WIDE, opts, &mut va) {
        Ok(v) => Some(v),
        Err(()) => {
            invalid();
            None
        }
    }
}

/// `__stdio_common_vsprintf` / `vswprintf`.
unsafe fn sprintf_impl<T: Unit>(
    opts: u64,
    buf: *mut T,
    size: usize,
    fmt: *const T,
    args: *const u8,
) -> i32 {
    if fmt.is_null() || (size != 0 && buf.is_null()) {
        return invalid();
    }
    let Some(out) = render(opts, fmt, args) else {
        return -1;
    };
    let n = out.len();
    let ni = n.min(i32::MAX as usize) as i32;
    let standard = opts & printf::STANDARD_SNPRINTF_BEHAVIOR != 0;
    if size == 0 {
        return if standard || n == 0 { ni } else { -1 };
    }
    if standard {
        let w = n.min(size - 1);
        put(buf, &out[..w]);
        *buf.add(w) = T::default();
        return ni;
    }
    if n < size {
        put(buf, &out);
        *buf.add(n) = T::default();
        return ni;
    }
    put(buf, &out[..size]);
    if opts & printf::LEGACY_VSPRINTF_NULL_TERMINATION != 0 {
        *buf.add(size - 1) = T::default();
    }
    if n == size {
        ni
    } else {
        -1
    }
}

/// `__stdio_common_vsprintf_s` / `vswprintf_s`.
unsafe fn sprintf_s_impl<T: Unit>(
    opts: u64,
    buf: *mut T,
    size: usize,
    fmt: *const T,
    args: *const u8,
) -> i32 {
    snprintf_s_impl(opts, buf, size, usize::MAX - 1, fmt, args)
}

/// `__stdio_common_vsnprintf_s` / `vsnwprintf_s`. `count == usize::MAX` is `_TRUNCATE`; the
/// sentinel `usize::MAX - 1` (never a real count here) means "no count" (`vsprintf_s`).
unsafe fn snprintf_s_impl<T: Unit>(
    opts: u64,
    buf: *mut T,
    size: usize,
    count: usize,
    fmt: *const T,
    args: *const u8,
) -> i32 {
    if buf.is_null() || size == 0 || fmt.is_null() {
        return invalid();
    }
    let Some(out) = render(opts, fmt, args) else {
        *buf = T::default();
        return -1;
    };
    let n = out.len();
    let truncating = count == usize::MAX || count < size;
    let limit = if count < size { count } else { size - 1 };
    if n <= limit {
        put(buf, &out);
        *buf.add(n) = T::default();
        return n.min(i32::MAX as usize) as i32;
    }
    if truncating {
        put(buf, &out[..limit]);
        *buf.add(limit) = T::default();
        return -1;
    }
    *buf = T::default();
    invalid_parameter_err(ERANGE);
    -1
}

unsafe fn fprintf_impl<T: Unit>(opts: u64, f: *mut File, fmt: *const T, args: *const u8) -> i32 {
    if f.is_null() || fmt.is_null() {
        return invalid();
    }
    let Some(out) = render(opts, fmt, args) else {
        return -1;
    };
    let bytes: Vec<u8> = if T::WIDE {
        printf::convert(&out, true, false)
            .iter()
            .map(|&c| c as u8)
            .collect()
    } else {
        out.iter().map(|&c| c as u8).collect()
    };
    if !write_all((*f).fd(), &bytes) {
        (*f).err.store(true, Ordering::Relaxed);
        return -1;
    }
    out.len().min(i32::MAX as usize) as i32
}

unsafe fn sscanf_impl<T: Unit>(
    opts: u64,
    input: *const T,
    len: usize,
    fmt: *const T,
    args: *const u8,
) -> i32 {
    if input.is_null() || fmt.is_null() {
        return invalid();
    }
    let inp = read_units(input, len);
    let f = read_format(fmt);
    let mut va = VaList::new(args);
    scanf::scan(&inp, &f, T::WIDE, opts, &mut va)
}

/// Host path of a stdio open, under an explicit write root: a path written with `/` is a host
/// path (as for reads elsewhere); a title path is written under `write_root` (parent
/// directories created) and read from an earlier write first, then from the package directories.
fn open_host_path_in(write_root: &Path, path: &[u8], write: bool) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(path);
    if text.is_empty() {
        return None;
    }
    if text.starts_with('/') {
        return Some(PathBuf::from(text.replace('\\', "/")));
    }
    if write {
        let out = crate::paths::map_write_path_in(write_root, &text)?;
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Some(out)
    } else {
        Some(
            crate::paths::map_write_path_in(write_root, &text)
                .filter(|p| p.exists())
                .unwrap_or_else(|| crate::paths::map_read_path(&text)),
        )
    }
}

/// [`open_host_path_in`] under the title's own write directory.
fn open_host_path(path: &[u8], write: bool) -> Option<PathBuf> {
    open_host_path_in(&crate::paths::write_dir(), path, write)
}

unsafe fn open_file(path: &[u8], mode: &[u8]) -> *mut File {
    let plus = mode.contains(&b'+');
    let flags = match mode.first() {
        Some(b'r') => {
            if plus {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            }
        }
        Some(b'w') => {
            (if plus { libc::O_RDWR } else { libc::O_WRONLY }) | libc::O_CREAT | libc::O_TRUNC
        }
        Some(b'a') => {
            (if plus { libc::O_RDWR } else { libc::O_WRONLY }) | libc::O_CREAT | libc::O_APPEND
        }
        _ => {
            invalid();
            return std::ptr::null_mut();
        }
    };
    // `w` and `a` create a file, which goes under the write directory; `r+` opens an existing
    // one, found the way reads are.
    let write = !matches!(mode.first(), Some(b'r'));
    let Some(host) = open_host_path(path, write) else {
        invalid();
        return std::ptr::null_mut();
    };
    let mut p = host.as_os_str().as_bytes().to_vec();
    p.push(0);
    let fd = libc::open(p.as_ptr() as *const c_char, flags | libc::O_CLOEXEC, 0o666);
    if fd < 0 {
        set_errno(os_errno());
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(File::new(fd)))
}

/// `fopen` / `_fsopen`: an ANSI path and mode.
///
/// # Safety
/// `path` and `mode` are NULL or NUL-terminated.
unsafe fn open_narrow(path: *const u8, mode: *const u8) -> *mut File {
    if path.is_null() || mode.is_null() {
        invalid();
        return std::ptr::null_mut();
    }
    let p: Vec<u8> = read_units(path, usize::MAX)
        .iter()
        .map(|&c| c as u8)
        .collect();
    let m: Vec<u8> = read_units(mode, usize::MAX)
        .iter()
        .map(|&c| c as u8)
        .collect();
    open_file(&p, &m)
}

/// `_wfopen` / `_wfsopen`: a UTF-16 path and an ANSI mode.
///
/// # Safety
/// `path` and `mode` are NULL or NUL-terminated.
unsafe fn open_wide(path: *const u16, mode: *const u16) -> *mut File {
    if path.is_null() || mode.is_null() {
        invalid();
        return std::ptr::null_mut();
    }
    let p: Vec<u8> = printf::convert(&read_units(path, usize::MAX), true, false)
        .iter()
        .map(|&c| c as u8)
        .collect();
    let m: Vec<u8> = read_units(mode, usize::MAX)
        .iter()
        .map(|&c| c as u8)
        .collect();
    open_file(&p, &m)
}

unsafe fn seek(f: *mut File, off: i64, origin: i32) -> i32 {
    if f.is_null() || !(0..=2).contains(&origin) {
        return invalid();
    }
    let f = &*f;
    let mut off = off;
    if origin == 1 && f.unget.load(Ordering::Relaxed) >= 0 {
        off -= 1;
    }
    f.unget.store(-1, Ordering::Relaxed);
    f.eof.store(false, Ordering::Relaxed);
    if libc::lseek(f.fd(), off, origin) < 0 {
        set_errno(os_errno());
        -1
    } else {
        0
    }
}

unsafe fn tell(f: *mut File) -> i64 {
    if f.is_null() {
        invalid();
        return -1;
    }
    let f = &*f;
    let p = libc::lseek(f.fd(), 0, libc::SEEK_CUR);
    if p < 0 {
        set_errno(os_errno());
        return -1;
    }
    p - i64::from(f.unget.load(Ordering::Relaxed) >= 0)
}

unsafe fn read_bytes(f: &File, buf: &mut [u8]) -> usize {
    let mut at = 0;
    if !buf.is_empty() {
        let u = f.unget.swap(-1, Ordering::Relaxed);
        if u >= 0 {
            buf[0] = u as u8;
            at = 1;
        }
    }
    match read_full(f.fd(), &mut buf[at..]) {
        Ok(n) => {
            if at + n < buf.len() {
                f.eof.store(true, Ordering::Relaxed);
            }
            at + n
        }
        Err(()) => {
            f.err.store(true, Ordering::Relaxed);
            at
        }
    }
}

win32! {
    module "api-ms-win-crt-stdio" => "ucrtbase";
    Native fn __p__commode() -> *mut i32 { COMMODE.as_ptr() }
    Native fn __p__fmode() -> *mut i32 { FMODE.as_ptr() }
    // Only `_O_TEXT` and `_O_BINARY` are accepted (`_O_WTEXT` and friends are not modelled).
    Native fn _set_fmode(mode: i32) -> i32 {
        if mode != O_TEXT && mode != O_BINARY {
            return invalid_parameter_err(EINVAL);
        }
        FMODE.store(mode, Ordering::Relaxed);
        0
    }
    Native fn _get_fmode(mode: *mut i32) -> i32 {
        if mode.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        *mode = FMODE.load(Ordering::Relaxed);
        0
    }

    // The `locale` and `args` parameters are the `_locale_t` and the Win64 `va_list`.
    Native fn __stdio_common_vsprintf(options: u64, buf: *mut u8, size: usize, fmt: *const u8, _locale: *const c_void, args: *const u8) -> i32 {
        sprintf_impl(options, buf, size, fmt, args)
    }
    Native fn __stdio_common_vsprintf_s(options: u64, buf: *mut u8, size: usize, fmt: *const u8, _locale: *const c_void, args: *const u8) -> i32 {
        sprintf_s_impl(options, buf, size, fmt, args)
    }
    Native fn __stdio_common_vsnprintf_s(options: u64, buf: *mut u8, size: usize, count: usize, fmt: *const u8, _locale: *const c_void, args: *const u8) -> i32 {
        snprintf_s_impl(options, buf, size, count, fmt, args)
    }
    Native fn __stdio_common_vswprintf(options: u64, buf: *mut u16, size: usize, fmt: *const u16, _locale: *const c_void, args: *const u8) -> i32 {
        sprintf_impl(options, buf, size, fmt, args)
    }
    Native fn __stdio_common_vswprintf_s(options: u64, buf: *mut u16, size: usize, fmt: *const u16, _locale: *const c_void, args: *const u8) -> i32 {
        sprintf_s_impl(options, buf, size, fmt, args)
    }
    Native fn __stdio_common_vsnwprintf_s(options: u64, buf: *mut u16, size: usize, count: usize, fmt: *const u16, _locale: *const c_void, args: *const u8) -> i32 {
        snprintf_s_impl(options, buf, size, count, fmt, args)
    }
    Native fn __stdio_common_vfprintf(options: u64, f: *mut File, fmt: *const u8, _locale: *const c_void, args: *const u8) -> i32 {
        fprintf_impl(options, f, fmt, args)
    }
    Native fn __stdio_common_vfwprintf(options: u64, f: *mut File, fmt: *const u16, _locale: *const c_void, args: *const u8) -> i32 {
        fprintf_impl(options, f, fmt, args)
    }
    Native fn __stdio_common_vsscanf(options: u64, input: *const u8, len: usize, fmt: *const u8, _locale: *const c_void, args: *const u8) -> i32 {
        sscanf_impl(options, input, len, fmt, args)
    }
    Native fn __stdio_common_vswscanf(options: u64, input: *const u16, len: usize, fmt: *const u16, _locale: *const c_void, args: *const u8) -> i32 {
        sscanf_impl(options, input, len, fmt, args)
    }

    Native fn __acrt_iob_func(index: u32) -> *mut File {
        match index {
            0 => &STDIN as *const File as *mut File,
            1 => &STDOUT as *const File as *mut File,
            2 => &STDERR as *const File as *mut File,
            _ => std::ptr::null_mut(),
        }
    }
    Native fn fopen(path: *const u8, mode: *const u8) -> *mut File {
        unsafe { open_narrow(path, mode) }
    }
    // The sharing mode (`_SH_DENYRW` and friends) is advisory here: one process, and no other
    // opener of a title file can hold a conflicting share.
    Native fn _fsopen(path: *const u8, mode: *const u8, _sharing: i32) -> *mut File {
        unsafe { open_narrow(path, mode) }
    }
    Native fn _wfopen(path: *const u16, mode: *const u16) -> *mut File {
        unsafe { open_wide(path, mode) }
    }
    Native fn _wfsopen(path: *const u16, mode: *const u16, _sharing: i32) -> *mut File {
        unsafe { open_wide(path, mode) }
    }
    Native fn fclose(f: *mut File) -> i32 {
        if f.is_null() {
            return invalid();
        }
        if is_std(f) {
            (*f).fd.store(-1, Ordering::Relaxed);
            return 0;
        }
        let b = Box::from_raw(f);
        if libc::close(b.fd()) < 0 {
            EOF
        } else {
            0
        }
    }
    Native fn fflush(f: *mut File) -> i32 {
        // Streams are unbuffered: nothing to flush (fflush(NULL) flushes every stream).
        let _ = f;
        0
    }
    Native fn setvbuf(f: *mut File, _buf: *mut c_char, mode: i32, size: usize) -> i32 {
        if f.is_null() || !matches!(mode, 0 | 0x40 | 4) || (mode != 4 && !(2..=i32::MAX as usize).contains(&size)) {
            return invalid();
        }
        0
    }
    Native fn fgetc(f: *mut File) -> i32 {
        if f.is_null() {
            return invalid();
        }
        let mut b = [0u8; 1];
        if read_bytes(&*f, &mut b) == 1 {
            b[0] as i32
        } else {
            EOF
        }
    }
    Native fn fputc(c: i32, f: *mut File) -> i32 {
        if f.is_null() {
            return invalid();
        }
        if write_all((*f).fd(), &[c as u8]) {
            c as u8 as i32
        } else {
            (*f).err.store(true, Ordering::Relaxed);
            EOF
        }
    }
    Native fn ungetc(c: i32, f: *mut File) -> i32 {
        if f.is_null() {
            return invalid();
        }
        let f = &*f;
        if c == EOF || f.unget.load(Ordering::Relaxed) >= 0 {
            return EOF;
        }
        f.unget.store(c & 0xff, Ordering::Relaxed);
        f.eof.store(false, Ordering::Relaxed);
        c & 0xff
    }
    Native fn fread(buf: *mut u8, size: usize, count: usize, f: *mut File) -> usize {
        if f.is_null() || (buf.is_null() && size != 0 && count != 0) {
            invalid();
            return 0;
        }
        let Some(total) = size.checked_mul(count).filter(|&t| t != 0) else { return 0 };
        let got = read_bytes(&*f, std::slice::from_raw_parts_mut(buf, total));
        got / size
    }
    Native fn fwrite(buf: *const u8, size: usize, count: usize, f: *mut File) -> usize {
        if f.is_null() || (buf.is_null() && size != 0 && count != 0) {
            invalid();
            return 0;
        }
        let Some(total) = size.checked_mul(count).filter(|&t| t != 0) else { return 0 };
        if write_all((*f).fd(), std::slice::from_raw_parts(buf, total)) {
            count
        } else {
            (*f).err.store(true, Ordering::Relaxed);
            0
        }
    }
    Native fn fseek(f: *mut File, offset: i32, origin: i32) -> i32 {
        seek(f, offset as i64, origin)
    }
    Native fn _fseeki64(f: *mut File, offset: i64, origin: i32) -> i32 {
        seek(f, offset, origin)
    }
    Native fn ftell(f: *mut File) -> i32 {
        let p = tell(f);
        if p > i32::MAX as i64 { -1 } else { p as i32 }
    }
    Native fn rewind(f: *mut File) {
        if seek(f, 0, 0) == 0 {
            (*f).err.store(false, Ordering::Relaxed);
        }
    }
    Native fn fgetpos(f: *mut File, pos: *mut i64) -> i32 {
        if pos.is_null() {
            return invalid();
        }
        let p = tell(f);
        if p < 0 {
            return -1;
        }
        *pos = p;
        0
    }
    Native fn fsetpos(f: *mut File, pos: *const i64) -> i32 {
        if pos.is_null() {
            return invalid();
        }
        seek(f, *pos, 0)
    }
    Native fn _get_stream_buffer_pointers(f: *mut File, base: *mut *mut usize, ptr: *mut *mut usize, count: *mut *mut i32) -> i32 {
        if f.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        if !base.is_null() {
            *base = NO_BUFFER.as_ptr();
        }
        if !ptr.is_null() {
            *ptr = NO_BUFFER.as_ptr();
        }
        if !count.is_null() {
            *count = NO_COUNT.as_ptr();
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_crt_runtime::{errno, test_support::*};

    #[test]
    fn registered_both_names() {
        for n in ["__p__commode", "__p__fmode", "_set_fmode", "_get_fmode"] {
            assert!(crate::lookup("api-ms-win-crt-stdio-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("ucrtbase", n).is_some());
        }
    }

    #[test]
    fn fmode_round_trip() {
        let _g = install_counting_handler();
        unsafe {
            let mut old = -1;
            assert_eq!(_get_fmode(&mut old), 0);
            assert_eq!(_set_fmode(O_BINARY), 0);
            let mut m = -1;
            assert_eq!(_get_fmode(&mut m), 0);
            assert_eq!(m, O_BINARY);
            assert_eq!(*__p__fmode(), O_BINARY);
            *__p__fmode() = O_TEXT;
            assert_eq!(_get_fmode(&mut m), 0);
            assert_eq!(m, O_TEXT);
            FMODE.store(old, Ordering::Relaxed);
        }
    }

    #[test]
    fn commode_is_writable_through_pointer() {
        unsafe {
            let p = __p__commode();
            assert_eq!(*p, 0);
            *p = 0x4000;
            assert_eq!(COMMODE.load(Ordering::Relaxed), 0x4000);
            *p = 0;
        }
    }

    #[test]
    fn fmode_errors() {
        let _g = install_counting_handler();
        unsafe {
            assert_eq!(_set_fmode(0x1234), EINVAL);
            assert_eq!(errno(), EINVAL);
            assert_eq!(_get_fmode(std::ptr::null_mut()), EINVAL);
            assert_eq!(calls(), 2);
        }
    }

    // --- printf / scanf family -------------------------------------------------------------

    use std::ptr::null;

    const STD: u64 = printf::STANDARD_SNPRINTF_BEHAVIOR;
    const WIDE_LEGACY: u64 = printf::LEGACY_WIDE_SPECIFIERS;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }
    fn cs(buf: &[u8]) -> String {
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
    fn ws(buf: &[u16]) -> String {
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }
    fn va(a: &[u64]) -> *const u8 {
        a.as_ptr() as *const u8
    }

    #[test]
    fn printf_family_registered() {
        for n in [
            "__stdio_common_vsprintf",
            "__stdio_common_vsprintf_s",
            "__stdio_common_vsnprintf_s",
            "__stdio_common_vswprintf",
            "__stdio_common_vswprintf_s",
            "__stdio_common_vsnwprintf_s",
            "__stdio_common_vfprintf",
            "__stdio_common_vfwprintf",
            "__stdio_common_vsscanf",
            "__stdio_common_vswscanf",
            "__acrt_iob_func",
            "fopen",
            "_fsopen",
            "_wfopen",
            "_wfsopen",
            "fclose",
            "fflush",
            "fgetc",
            "fputc",
            "fread",
            "fwrite",
            "fseek",
            "_fseeki64",
            "ftell",
            "rewind",
            "fgetpos",
            "fsetpos",
            "ungetc",
            "setvbuf",
            "_get_stream_buffer_pointers",
        ] {
            assert!(
                crate::lookup("api-ms-win-crt-stdio-l1-1-0.dll", n).is_some(),
                "{n}"
            );
        }
    }

    #[test]
    fn vsprintf_legacy_truncation() {
        let _g = install_counting_handler();
        let a = [5u64];
        unsafe {
            let mut b = [0xffu8; 8];
            // fits with the terminator
            assert_eq!(
                __stdio_common_vsprintf(
                    0,
                    b.as_mut_ptr(),
                    8,
                    c"ab%d".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                3
            );
            assert_eq!(cs(&b), "ab5");
            // exactly full: count returned, no terminator
            let mut b = [0xffu8; 8];
            assert_eq!(
                __stdio_common_vsprintf(
                    0,
                    b.as_mut_ptr(),
                    8,
                    c"justfits".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                8
            );
            assert_eq!(&b, b"justfits");
            // too long: -1, buffer filled
            let mut b = [0xffu8; 8];
            assert_eq!(
                __stdio_common_vsprintf(
                    0,
                    b.as_mut_ptr(),
                    8,
                    c"muchlonger".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(&b, b"muchlong");
            // legacy null termination option
            let mut b = [0xffu8; 8];
            let o = printf::LEGACY_VSPRINTF_NULL_TERMINATION;
            assert_eq!(
                __stdio_common_vsprintf(
                    o,
                    b.as_mut_ptr(),
                    8,
                    c"muchlonger".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(&b, b"muchlon\0");
            assert_eq!(calls(), 0);
        }
    }

    #[test]
    fn vsprintf_standard_and_size_query() {
        let _g = install_counting_handler();
        let a = [123u64];
        unsafe {
            let mut b = [0xffu8; 4];
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    b.as_mut_ptr(),
                    4,
                    c"AB%dC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                6
            );
            assert_eq!(cs(&b), "AB1");
            // size query: null buffer, count 0
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    std::ptr::null_mut(),
                    0,
                    c"AB%dC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                6
            );
            let mut wb = [0xffffu16; 4];
            assert_eq!(
                __stdio_common_vswprintf(
                    STD,
                    wb.as_mut_ptr(),
                    4,
                    w("AB%dC").as_ptr(),
                    null(),
                    va(&a)
                ),
                6
            );
            assert_eq!(ws(&wb), "AB1");
            assert_eq!(
                __stdio_common_vswprintf(
                    STD,
                    std::ptr::null_mut(),
                    0,
                    w("%d").as_ptr(),
                    null(),
                    va(&a)
                ),
                3
            );
            assert_eq!(calls(), 0);
        }
    }

    #[test]
    fn vsprintf_invalid_parameters() {
        let _g = install_counting_handler();
        let a = [1u64];
        unsafe {
            let mut b = [0u8; 8];
            // invalid conversion, %n, null format, null buffer with a count
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    b.as_mut_ptr(),
                    8,
                    c"%y".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(errno(), EINVAL);
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    b.as_mut_ptr(),
                    8,
                    c"%n".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(
                __stdio_common_vsprintf(STD, b.as_mut_ptr(), 8, null(), null(), va(&a)),
                -1
            );
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    std::ptr::null_mut(),
                    8,
                    c"x".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(calls(), 4);
            // wide
            assert_eq!(
                __stdio_common_vswprintf(
                    STD,
                    std::ptr::null_mut(),
                    8,
                    w("x").as_ptr(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(
                __stdio_common_vswprintf(
                    STD,
                    std::ptr::null_mut(),
                    0,
                    w("%z0").as_ptr(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(calls(), 6);
        }
    }

    #[test]
    fn vsprintf_s_variants() {
        let _g = install_counting_handler();
        let a = [123u64];
        unsafe {
            let mut b = [0xffu8; 8];
            assert_eq!(
                __stdio_common_vsprintf_s(
                    0,
                    b.as_mut_ptr(),
                    8,
                    c"AB%dC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                6
            );
            assert_eq!(cs(&b), "AB123C");
            assert_eq!(calls(), 0);
            // does not fit: empty string, ERANGE, handler called
            assert_eq!(
                __stdio_common_vsprintf_s(
                    0,
                    b.as_mut_ptr(),
                    6,
                    c"AB%dC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(b[0], 0);
            assert_eq!(errno(), ERANGE);
            assert_eq!(calls(), 1);
            // zero size / null buffer
            assert_eq!(
                __stdio_common_vsprintf_s(
                    0,
                    b.as_mut_ptr(),
                    0,
                    c"x".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(
                __stdio_common_vsprintf_s(
                    0,
                    std::ptr::null_mut(),
                    4,
                    c"x".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(calls(), 3);
            // wide
            let mut wb = [0xffffu16; 8];
            assert_eq!(
                __stdio_common_vswprintf_s(
                    WIDE_LEGACY,
                    wb.as_mut_ptr(),
                    8,
                    w("%s|%d").as_ptr(),
                    null(),
                    va(&[w("é").as_ptr() as u64, 4])
                ),
                3
            );
            assert_eq!(ws(&wb), "\u{e9}|4");
        }
    }

    #[test]
    fn vsnprintf_s_truncate_and_count() {
        let _g = install_counting_handler();
        let a = [123u64];
        let t = usize::MAX;
        unsafe {
            let mut wb = [0u16; 14];
            // Wine's _vsnwprintf_s cases.
            for (size, ret, text) in [
                (14, 6, "AB123C"),
                (12, 6, "AB123C"),
                (7, 6, "AB123C"),
                (6, -1, "AB123"),
                (2, -1, "A"),
                (1, -1, ""),
            ] {
                assert_eq!(
                    __stdio_common_vsnwprintf_s(
                        0,
                        wb.as_mut_ptr(),
                        size,
                        t,
                        w("AB%uC").as_ptr(),
                        null(),
                        va(&a)
                    ),
                    ret,
                    "size {size}"
                );
                assert_eq!(ws(&wb), text);
            }
            assert_eq!(calls(), 0);
            let mut b = [0xffu8; 10];
            // count below the size: truncated to count, -1
            assert_eq!(
                __stdio_common_vsnprintf_s(
                    0,
                    b.as_mut_ptr(),
                    10,
                    3,
                    c"AB%uC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(cs(&b), "AB1");
            assert_eq!(calls(), 0);
            // count large enough
            assert_eq!(
                __stdio_common_vsnprintf_s(
                    0,
                    b.as_mut_ptr(),
                    10,
                    8,
                    c"AB%uC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                6
            );
            assert_eq!(cs(&b), "AB123C");
            // count >= size and no room: invalid parameter
            assert_eq!(
                __stdio_common_vsnprintf_s(
                    0,
                    b.as_mut_ptr(),
                    4,
                    8,
                    c"AB%uC".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(b[0], 0);
            assert_eq!(calls(), 1);
        }
    }

    #[test]
    fn wide_specifiers_through_the_export() {
        let _g = install_counting_handler();
        let narrow = c"nar";
        let wide = w("wid");
        unsafe {
            let mut wb = [0u16; 16];
            let a = [wide.as_ptr() as u64, narrow.as_ptr() as u64];
            assert_eq!(
                __stdio_common_vswprintf(
                    STD | WIDE_LEGACY,
                    wb.as_mut_ptr(),
                    16,
                    w("%s %hs").as_ptr(),
                    null(),
                    va(&a)
                ),
                7
            );
            assert_eq!(ws(&wb), "wid nar");
            let a = [narrow.as_ptr() as u64, wide.as_ptr() as u64];
            assert_eq!(
                __stdio_common_vswprintf(
                    STD,
                    wb.as_mut_ptr(),
                    16,
                    w("%s %ls").as_ptr(),
                    null(),
                    va(&a)
                ),
                7
            );
            assert_eq!(ws(&wb), "nar wid");
            let mut b = [0u8; 16];
            let a = [wide.as_ptr() as u64];
            assert_eq!(
                __stdio_common_vsprintf(
                    STD,
                    b.as_mut_ptr(),
                    16,
                    c"%ls|%S".as_ptr().cast(),
                    null(),
                    va(&[a[0], a[0]])
                ),
                7
            );
            assert_eq!(cs(&b), "wid|wid");
        }
    }

    #[test]
    fn std_streams_write_and_files_round_trip() {
        let _g = install_counting_handler();
        unsafe {
            assert_eq!(__acrt_iob_func(1), &STDOUT as *const File as *mut File);
            assert_eq!((*__acrt_iob_func(2)).fd(), 2);
            assert!(__acrt_iob_func(3).is_null());
            let path =
                std::env::temp_dir().join(format!("nativ64-stdio-{}.tmp", std::process::id()));
            let mut p = path.to_str().unwrap().as_bytes().to_vec();
            p.push(0);
            let f = fopen(p.as_ptr(), c"wb".as_ptr().cast());
            assert!(!f.is_null());
            let a = [7u64, 0];
            assert_eq!(
                __stdio_common_vfprintf(0, f, c"n=%d;".as_ptr().cast(), null(), va(&a)),
                4
            );
            assert_eq!(
                __stdio_common_vfwprintf(0, f, w("w=%d").as_ptr(), null(), va(&a)),
                3
            );
            assert_eq!(fputc(b'!' as i32, f), b'!' as i32);
            assert_eq!(fwrite(b"xy".as_ptr(), 1, 2, f), 2);
            assert_eq!(ftell(f), 4 + 3 + 1 + 2);
            assert_eq!(fflush(f), 0);
            assert_eq!(fclose(f), 0);

            let f = fopen(p.as_ptr(), c"rb".as_ptr().cast());
            assert!(!f.is_null());
            let mut buf = [0u8; 32];
            assert_eq!(fgetc(f), b'n' as i32);
            assert_eq!(ungetc(b'Q' as i32, f), b'Q' as i32);
            assert_eq!(ftell(f), 0);
            assert_eq!(fread(buf.as_mut_ptr(), 1, 32, f), 10);
            assert_eq!(cs(&buf), "Q=7;w=7!xy");
            assert_eq!(fgetc(f), -1);
            assert_eq!(fseek(f, 2, 0), 0);
            assert_eq!(fgetc(f), b'7' as i32);
            assert_eq!(_fseeki64(f, -1, 2), 0);
            assert_eq!(fgetc(f), b'y' as i32);
            let mut pos = 0i64;
            assert_eq!(fgetpos(f, &mut pos), 0);
            assert_eq!(pos, 10);
            rewind(f);
            assert_eq!(ftell(f), 0);
            assert_eq!(fsetpos(f, &4), 0);
            assert_eq!(fgetc(f), b'w' as i32);
            assert_eq!(fclose(f), 0);
            let _ = std::fs::remove_file(path);

            // errors
            assert!(fopen(c"/nonexistent-dir/x".as_ptr().cast(), c"r".as_ptr().cast()).is_null());
            assert_eq!(errno(), 2);
            assert!(fopen(c"x".as_ptr().cast(), c"z".as_ptr().cast()).is_null());
            assert_eq!(fgetc(std::ptr::null_mut()), -1);
            assert_eq!(fseek(f_null(), 0, 0), -1);
            assert_eq!(
                __stdio_common_vfprintf(
                    0,
                    std::ptr::null_mut(),
                    c"x".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(setvbuf(__acrt_iob_func(1), std::ptr::null_mut(), 4, 0), 0);
            assert_eq!(calls(), 4);
        }
    }

    fn f_null() -> *mut File {
        std::ptr::null_mut()
    }

    #[test]
    fn fsopen_opens_like_fopen_and_ignores_the_sharing_mode() {
        let _g = install_counting_handler();
        unsafe {
            let path =
                std::env::temp_dir().join(format!("nativ64-fsopen-{}.tmp", std::process::id()));
            let mut p = path.to_str().unwrap().as_bytes().to_vec();
            p.push(0);
            // _SH_DENYRW is advisory here; a single process opens its own files.
            let f = _fsopen(p.as_ptr(), c"w+b".as_ptr().cast(), 0x10);
            assert!(!f.is_null());
            assert_eq!(fwrite(b"abc".as_ptr(), 1, 3, f), 3);
            assert_eq!(fclose(f), 0);
            let wp = w(path.to_str().unwrap());
            let f = _wfsopen(wp.as_ptr(), w("rb").as_ptr(), 0x10);
            assert!(!f.is_null());
            let mut b = [0u8; 4];
            assert_eq!(fread(b.as_mut_ptr(), 1, 4, f), 3);
            assert_eq!(&b[..3], b"abc");
            assert_eq!(fclose(f), 0);
            std::fs::remove_file(&path).unwrap();
            // Errors: a NULL argument is the usual invalid-parameter failure.
            assert!(_fsopen(std::ptr::null(), c"r".as_ptr().cast(), 0).is_null());
            assert!(_wfsopen(wp.as_ptr(), std::ptr::null(), 0).is_null());
            assert_eq!(errno(), EINVAL);
            assert_eq!(calls(), 2);
            assert!(crate::lookup("api-ms-win-crt-stdio-l1-1-0.dll", "_wfsopen").is_some());
        }
    }

    #[test]
    fn stdio_paths_follow_the_file_api_mapping() {
        let root = std::env::temp_dir().join(format!("nativ64-stdio-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A title path (here the console's `t:` scratch drive): the drive is dropped and the
        // missing parent directories are created under the write directory.
        let p = open_host_path_in(&root, br"t:\wer\crash-1.log", true).unwrap();
        assert_eq!(p, root.join("wer").join("crash-1.log"));
        assert!(p.parent().unwrap().is_dir());
        std::fs::write(&p, b"report").unwrap();
        // The same title path reads that copy back before searching the package directories.
        assert_eq!(
            open_host_path_in(&root, br"t:\wer\crash-1.log", false),
            Some(p.clone())
        );
        // A path written with `/` is a host path, whatever the mode.
        assert_eq!(
            open_host_path_in(&root, b"/tmp/nativ64-host.bin", true),
            Some(PathBuf::from("/tmp/nativ64-host.bin"))
        );
        // Nothing to map: an empty path, and a drive prefix alone.
        assert_eq!(open_host_path_in(&root, b"", true), None);
        assert_eq!(open_host_path_in(&root, b"t:", true), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn vfprintf_writes_to_a_pipe_as_stdout_does() {
        // A stream over a pipe stands in for stdout/stderr (same code path, fd 1 and 2).
        unsafe {
            let mut fds = [0i32; 2];
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            let f = Box::into_raw(Box::new(File::new(fds[1])));
            let we = w("é");
            let a = [we.as_ptr() as u64, 3];
            assert_eq!(
                __stdio_common_vfprintf(0, f, c"%S=%d\n".as_ptr().cast(), null(), va(&a)),
                5
            );
            let mut b = [0u8; 16];
            let n = libc::read(fds[0], b.as_mut_ptr().cast(), 16);
            assert_eq!(&b[..n as usize], "é=3\n".as_bytes());
            assert_eq!(fclose(f), 0);
            libc::close(fds[0]);
        }
    }

    #[test]
    fn sscanf_exports() {
        let _g = install_counting_handler();
        unsafe {
            let (mut x, mut y) = (0i32, 0f32);
            let a = [&mut x as *mut i32 as u64, &mut y as *mut f32 as u64];
            assert_eq!(
                __stdio_common_vsscanf(
                    0,
                    c"12 3.5".as_ptr().cast(),
                    usize::MAX,
                    c"%d %f".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                2
            );
            assert_eq!((x, y), (12, 3.5));
            let wi = w("7 z");
            let mut ch = [0u16; 2];
            let a = [&mut x as *mut i32 as u64, ch.as_mut_ptr() as u64];
            assert_eq!(
                __stdio_common_vswscanf(
                    0,
                    wi.as_ptr(),
                    usize::MAX,
                    w("%d %c").as_ptr(),
                    null(),
                    va(&a)
                ),
                2
            );
            assert_eq!((x, ch[0]), (7, b'z' as u16));
            assert_eq!(
                __stdio_common_vsscanf(
                    0,
                    c"".as_ptr().cast(),
                    usize::MAX,
                    c"%d".as_ptr().cast(),
                    null(),
                    va(&a)
                ),
                -1
            );
            assert_eq!(
                __stdio_common_vsscanf(0, null(), 0, c"%d".as_ptr().cast(), null(), va(&a)),
                -1
            );
            assert_eq!(calls(), 1);
        }
    }
}
