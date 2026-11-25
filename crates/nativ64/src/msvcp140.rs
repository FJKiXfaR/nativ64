//! `msvcp140`: the parts of the MSVC C++ standard library DLL that are plain host services
//! (system_error tables, `std::mutex` storage, `<chrono>` clocks, `random_device`, `_Yarn`).
//! Semantics and layouts follow Microsoft's STL (Apache-2.0 WITH LLVM-exception):
//! `stl/src/{syserror,mutex,xtime,xrngdev,thread0,xthrow}.cpp`, `stl/inc/{xthreads.h,
//! __msvc_threads_core.hpp,xlocinfo}`.
//!
//! Exported names are MSVC-decorated, so the table is written by hand instead of with `win32!`.
//!
//! `_Mtx_internal_imp_t` (80 bytes, 8-aligned): `int _Type` at 0, the 64-byte critical section
//! at 8 (we keep our own futex word at offset 16, where the STL keeps its SRW lock), the owner
//! thread id at 72 and the recursion count at 76. A mutex whose type minus `_Mtx_recursive`
//! is `_Mtx_plain` is recursive in practice (as in the STL); other types fail a relock with
//! `_Busy`.
//!
//! The `_X*` throwers cannot throw yet: the loader has no C++ exceptions. They print
//! `nativ64: C++ exception std::<kind>: <msg>` and abort.
//! TODO: raise the real exception through `_CxxThrowException` once SEH/C++ EH exists.

#![allow(clippy::missing_safety_doc)]

use crate::{Export, Status};
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

// ---------------------------------------------------------------- system_error tables

mod e {
    // Windows UCRT errno values (errc).
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const ESRCH: i32 = 3;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const E2BIG: i32 = 7;
    pub const ENOEXEC: i32 = 8;
    pub const EBADF: i32 = 9;
    pub const ECHILD: i32 = 10;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EFAULT: i32 = 14;
    pub const EBUSY: i32 = 16;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENODEV: i32 = 19;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const ENFILE: i32 = 23;
    pub const EMFILE: i32 = 24;
    pub const ENOTTY: i32 = 25;
    pub const EFBIG: i32 = 27;
    pub const ENOSPC: i32 = 28;
    pub const ESPIPE: i32 = 29;
    pub const EROFS: i32 = 30;
    pub const EMLINK: i32 = 31;
    pub const EPIPE: i32 = 32;
    pub const EDOM: i32 = 33;
    pub const ERANGE: i32 = 34;
    pub const EDEADLK: i32 = 36;
    pub const ENAMETOOLONG: i32 = 38;
    pub const ENOLCK: i32 = 39;
    pub const ENOSYS: i32 = 40;
    pub const ENOTEMPTY: i32 = 41;
    pub const EILSEQ: i32 = 42;
    pub const EADDRINUSE: i32 = 100;
    pub const EADDRNOTAVAIL: i32 = 101;
    pub const EAFNOSUPPORT: i32 = 102;
    pub const EALREADY: i32 = 103;
    pub const EBADMSG: i32 = 104;
    pub const ECANCELED: i32 = 105;
    pub const ECONNABORTED: i32 = 106;
    pub const ECONNREFUSED: i32 = 107;
    pub const ECONNRESET: i32 = 108;
    pub const EDESTADDRREQ: i32 = 109;
    pub const EHOSTUNREACH: i32 = 110;
    pub const EIDRM: i32 = 111;
    pub const EINPROGRESS: i32 = 112;
    pub const EISCONN: i32 = 113;
    pub const ELOOP: i32 = 114;
    pub const EMSGSIZE: i32 = 115;
    pub const ENETDOWN: i32 = 116;
    pub const ENETRESET: i32 = 117;
    pub const ENETUNREACH: i32 = 118;
    pub const ENOBUFS: i32 = 119;
    pub const ENODATA: i32 = 120;
    pub const ENOLINK: i32 = 121;
    pub const ENOMSG: i32 = 122;
    pub const ENOPROTOOPT: i32 = 123;
    pub const ENOSR: i32 = 124;
    pub const ENOSTR: i32 = 125;
    pub const ENOTCONN: i32 = 126;
    pub const ENOTRECOVERABLE: i32 = 127;
    pub const ENOTSOCK: i32 = 128;
    pub const ENOTSUP: i32 = 129;
    pub const EOPNOTSUPP: i32 = 130;
    pub const EOVERFLOW: i32 = 132;
    pub const EOWNERDEAD: i32 = 133;
    pub const EPROTO: i32 = 134;
    pub const EPROTONOSUPPORT: i32 = 135;
    pub const EPROTOTYPE: i32 = 136;
    pub const ETIME: i32 = 137;
    pub const ETIMEDOUT: i32 = 138;
    pub const ETXTBSY: i32 = 139;
    pub const EWOULDBLOCK: i32 = 140;
}
use e::*;

/// Windows error -> errc, sorted by Windows code (`_Win_errtab`).
static WIN_ERRTAB: &[(i32, i32)] = &[
    (1, ENOSYS),
    (2, ENOENT),
    (3, ENOENT),
    (4, EMFILE),
    (5, EACCES),
    (6, EINVAL),
    (8, ENOMEM),
    (12, EACCES),
    (14, ENOMEM),
    (15, ENODEV),
    (16, EACCES),
    (17, EXDEV),
    (19, EACCES),
    (20, ENODEV),
    (21, EAGAIN),
    (25, EIO),
    (29, EIO),
    (30, EIO),
    (32, EACCES),
    (33, ENOLCK),
    (39, ENOSPC),
    (50, ENOTSUP),
    (53, ENOENT),
    (55, ENODEV),
    (67, ENOENT),
    (80, EEXIST),
    (82, EACCES),
    (87, EINVAL),
    (109, EPIPE),
    (110, EIO),
    (111, ENAMETOOLONG),
    (112, ENOSPC),
    (121, ETIMEDOUT),
    (123, ENOENT),
    (131, EINVAL),
    (142, EBUSY),
    (145, ENOTEMPTY),
    (170, EBUSY),
    (183, EEXIST),
    (206, ENAMETOOLONG),
    (212, ENOLCK),
    (258, ETIMEDOUT),
    (267, EINVAL),
    (995, ECANCELED),
    (998, EACCES),
    (1011, EIO),
    (1012, EIO),
    (1013, EIO),
    (1237, EAGAIN),
    (1460, ETIMEDOUT),
    (2401, EBUSY),
    (2404, EBUSY),
    (4393, EINVAL),
    (10004, EINTR),
    (10009, EBADF),
    (10013, EACCES),
    (10014, EFAULT),
    (10022, EINVAL),
    (10024, EMFILE),
    (10035, EWOULDBLOCK),
    (10036, EINPROGRESS),
    (10037, EALREADY),
    (10038, ENOTSOCK),
    (10039, EDESTADDRREQ),
    (10040, EMSGSIZE),
    (10041, EPROTOTYPE),
    (10042, ENOPROTOOPT),
    (10043, EPROTONOSUPPORT),
    (10045, EOPNOTSUPP),
    (10047, EAFNOSUPPORT),
    (10048, EADDRINUSE),
    (10049, EADDRNOTAVAIL),
    (10050, ENETDOWN),
    (10051, ENETUNREACH),
    (10052, ENETRESET),
    (10053, ECONNABORTED),
    (10054, ECONNRESET),
    (10055, ENOBUFS),
    (10056, EISCONN),
    (10057, ENOTCONN),
    (10060, ETIMEDOUT),
    (10061, ECONNREFUSED),
    (10063, ENAMETOOLONG),
    (10065, EHOSTUNREACH),
];

/// errc -> message (`_Sys_errtab`, order of the STL; errc names noted per line where they differ
/// from the errno name).
static SYS_ERRTAB: &[(i32, &CStr)] = &[
    (0, c"success"),
    (EAFNOSUPPORT, c"address family not supported"),
    (EADDRINUSE, c"address in use"),
    (EADDRNOTAVAIL, c"address not available"),
    (EISCONN, c"already connected"),
    (E2BIG, c"argument list too long"),
    (EDOM, c"argument out of domain"),
    (EFAULT, c"bad address"),
    (EBADF, c"bad file descriptor"),
    (EBADMSG, c"bad message"),
    (EPIPE, c"broken pipe"),
    (ECONNABORTED, c"connection aborted"),
    (EALREADY, c"connection already in progress"),
    (ECONNREFUSED, c"connection refused"),
    (ECONNRESET, c"connection reset"),
    (EXDEV, c"cross device link"),
    (EDESTADDRREQ, c"destination address required"),
    (EBUSY, c"device or resource busy"),
    (ENOTEMPTY, c"directory not empty"),
    (ENOEXEC, c"executable format error"),
    (EEXIST, c"file exists"),
    (EFBIG, c"file too large"),
    (ENAMETOOLONG, c"filename too long"),
    (ENOSYS, c"function not supported"),
    (EHOSTUNREACH, c"host unreachable"),
    (EIDRM, c"identifier removed"),
    (EILSEQ, c"illegal byte sequence"),
    (ENOTTY, c"inappropriate io control operation"),
    (EINTR, c"interrupted"),
    (EINVAL, c"invalid argument"),
    (ESPIPE, c"invalid seek"),
    (EIO, c"io error"),
    (EISDIR, c"is a directory"),
    (EMSGSIZE, c"message size"),
    (ENETDOWN, c"network down"),
    (ENETRESET, c"network reset"),
    (ENETUNREACH, c"network unreachable"),
    (ENOBUFS, c"no buffer space"),
    (ECHILD, c"no child process"),
    (ENOLINK, c"no link"),
    (ENOLCK, c"no lock available"),
    (ENODATA, c"no message available"),
    (ENOMSG, c"no message"),
    (ENOPROTOOPT, c"no protocol option"),
    (ENOSPC, c"no space on device"),
    (ENOSR, c"no stream resources"),
    (ENXIO, c"no such device or address"),
    (ENODEV, c"no such device"),
    (ENOENT, c"no such file or directory"),
    (ESRCH, c"no such process"),
    (ENOTDIR, c"not a directory"),
    (ENOTSOCK, c"not a socket"),
    (ENOSTR, c"not a stream"),
    (ENOTCONN, c"not connected"),
    (ENOMEM, c"not enough memory"),
    (ENOTSUP, c"not supported"),
    (ECANCELED, c"operation canceled"),
    (EINPROGRESS, c"operation in progress"),
    (EPERM, c"operation not permitted"),
    (EOPNOTSUPP, c"operation not supported"),
    (EWOULDBLOCK, c"operation would block"),
    (EOWNERDEAD, c"owner dead"),
    (EACCES, c"permission denied"),
    (EPROTO, c"protocol error"),
    (EPROTONOSUPPORT, c"protocol not supported"),
    (EROFS, c"read only file system"),
    (EDEADLK, c"resource deadlock would occur"),
    (EAGAIN, c"resource unavailable try again"),
    (ERANGE, c"result out of range"),
    (ENOTRECOVERABLE, c"state not recoverable"),
    (ETIME, c"stream timeout"),
    (ETXTBSY, c"text file busy"),
    (ETIMEDOUT, c"timed out"),
    (ENFILE, c"too many files open in system"),
    (EMFILE, c"too many files open"),
    (EMLINK, c"too many links"),
    (ELOOP, c"too many symbolic link levels"),
    (EOVERFLOW, c"value too large"),
    (EPROTOTYPE, c"wrong protocol type"),
];

fn winerror_map(code: i32) -> i32 {
    WIN_ERRTAB
        .iter()
        .find(|&&(w, _)| w == code)
        .map_or(0, |&(_, p)| p)
}

fn syserror_map(code: i32) -> &'static CStr {
    SYS_ERRTAB
        .iter()
        .find(|&&(c, _)| c == code)
        .map_or(c"unknown error", |&(_, m)| m)
}

// ---------------------------------------------------------------- throwers

/// Reports the C++ exception the title tried to throw, then aborts.
fn fatal(kind: &str, msg: &str) -> ! {
    // TODO: raise the real exception through `_CxxThrowException` once C++ EH exists.
    eprintln!("{}", exception_text(kind, msg));
    std::process::abort()
}

fn exception_text(kind: &str, msg: &str) -> String {
    if msg.is_empty() {
        format!("nativ64: C++ exception std::{kind}")
    } else {
        format!("nativ64: C++ exception std::{kind}: {msg}")
    }
}

unsafe fn text(msg: *const c_char) -> String {
    if msg.is_null() {
        String::new()
    } else {
        CStr::from_ptr(msg).to_string_lossy().into_owned()
    }
}

/// `_Throw_C_error`: `(exception kind, message)` for a `_Thrd_result`, None when the STL aborts.
fn c_error_exception(code: i32) -> Option<(&'static str, &'static str)> {
    match code {
        1 | 2 => Some(("system_error", "resource unavailable try again")),
        3 => Some(("system_error", "device or resource busy")),
        4 => Some(("system_error", "invalid argument")),
        _ => None,
    }
}

pub unsafe extern "win64" fn xbad_alloc() {
    fatal("bad_alloc", "bad allocation")
}
pub unsafe extern "win64" fn xbad_function_call() {
    fatal("bad_function_call", "bad function call")
}
pub unsafe extern "win64" fn xinvalid_argument(msg: *const c_char) {
    fatal("invalid_argument", &text(msg))
}
pub unsafe extern "win64" fn xlength_error(msg: *const c_char) {
    fatal("length_error", &text(msg))
}
pub unsafe extern "win64" fn xout_of_range(msg: *const c_char) {
    fatal("out_of_range", &text(msg))
}
pub unsafe extern "win64" fn xruntime_error(msg: *const c_char) {
    fatal("runtime_error", &text(msg))
}
pub unsafe extern "win64" fn throw_c_error(code: i32) {
    match c_error_exception(code) {
        Some((kind, msg)) => fatal(kind, msg),
        None => std::process::abort(),
    }
}

// ---------------------------------------------------------------- misc services

pub unsafe extern "win64" fn random_device() -> u32 {
    let mut v = 0u32;
    let n = libc::getrandom(std::ptr::addr_of_mut!(v).cast(), 4, 0);
    if n != 4 {
        fatal("out_of_range", "invalid random_device value");
    }
    v
}

pub unsafe extern "win64" fn syserror_map_export(code: i32) -> *const c_char {
    syserror_map(code).as_ptr()
}

pub unsafe extern "win64" fn winerror_map_export(code: i32) -> i32 {
    winerror_map(code)
}

/// No SEH yet, so no exception is ever in flight.
pub unsafe extern "win64" fn uncaught_exception() -> bool {
    false
}

pub unsafe extern "win64" fn query_perf_counter() -> i64 {
    (crate::api_ms_win_core_profile::monotonic_ns() / 100) as i64
}

pub unsafe extern "win64" fn query_perf_frequency() -> i64 {
    10_000_000
}

/// 100 ns intervals since 1970-01-01.
pub unsafe extern "win64" fn xtime_get_ticks() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    ts.tv_sec * 10_000_000 + ts.tv_nsec / 100
}

// ---------------------------------------------------------------- _Yarn<char>

/// `std::_Yarn<char>`: `char* _Myptr; char _Nul;`.
#[repr(C)]
pub struct YarnChar {
    ptr: *mut c_char,
    nul: c_char,
}

/// `_Yarn<char>::operator=(const char*)`: frees the old copy, duplicates `src` with `malloc`
/// (so the title's inline `free` matches).
pub unsafe extern "win64" fn yarn_assign(this: *mut YarnChar, src: *const c_char) -> *mut YarnChar {
    if !std::ptr::eq((*this).ptr, src) {
        libc::free((*this).ptr.cast());
        (*this).ptr = std::ptr::null_mut();
        if !src.is_null() {
            let n = libc::strlen(src) + 1;
            let p = libc::malloc(n) as *mut c_char;
            if p.is_null() {
                fatal("bad_alloc", "bad allocation");
            }
            std::ptr::copy_nonoverlapping(src, p, n);
            (*this).ptr = p;
        }
    }
    this
}

// ---------------------------------------------------------------- _Mtx_*_in_situ

const MTX_PLAIN: i32 = 1;
const MTX_RECURSIVE: i32 = 0x100;
const THRD_SUCCESS: i32 = 0;
const THRD_BUSY: i32 = 3;

const OFF_TYPE: usize = 0;
const OFF_LOCK: usize = 16;
const OFF_TID: usize = 72;
const OFF_COUNT: usize = 76;
/// Size of `_Mtx_internal_imp_t`.
pub const MTX_SIZE: usize = 80;

unsafe fn field_i32<'a>(m: *mut u8, off: usize) -> &'a AtomicI32 {
    &*(m.add(off) as *const AtomicI32)
}

unsafe fn lock_word<'a>(m: *mut u8) -> &'a AtomicU32 {
    &*(m.add(OFF_LOCK) as *const AtomicU32)
}

fn futex(word: &AtomicU32, op: i32, val: u32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32,
            op | libc::FUTEX_PRIVATE_FLAG,
            val,
            std::ptr::null::<libc::timespec>(),
        );
    }
}

/// Futex mutex: 0 free, 1 locked, 2 locked with waiters.
fn raw_lock(w: &AtomicU32) {
    if w.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    while w.swap(2, Ordering::Acquire) != 0 {
        futex(w, libc::FUTEX_WAIT, 2);
    }
}

fn raw_unlock(w: &AtomicU32) {
    if w.swap(0, Ordering::Release) == 2 {
        futex(w, libc::FUTEX_WAKE, 1);
    }
}

fn current_tid() -> i32 {
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

pub unsafe extern "win64" fn mtx_init_in_situ(m: *mut u8, kind: i32) {
    std::ptr::write_bytes(m.add(8), 0, 64);
    field_i32(m, OFF_TID).store(-1, Ordering::Relaxed);
    field_i32(m, OFF_TYPE).store(kind, Ordering::Relaxed);
    field_i32(m, OFF_COUNT).store(0, Ordering::Relaxed);
}

pub unsafe extern "win64" fn mtx_destroy_in_situ(_m: *mut u8) {}

pub unsafe extern "win64" fn mtx_lock(m: *mut u8) -> i32 {
    let tid = current_tid();
    let kind = field_i32(m, OFF_TYPE).load(Ordering::Relaxed);
    let (owner, count) = (field_i32(m, OFF_TID), field_i32(m, OFF_COUNT));
    if owner.load(Ordering::Relaxed) != tid {
        raw_lock(lock_word(m));
    }
    if kind & !MTX_RECURSIVE == MTX_PLAIN {
        owner.store(tid, Ordering::Relaxed);
        count.fetch_add(1, Ordering::Relaxed);
        return THRD_SUCCESS;
    }
    // Timed/try mutex: recursive only when asked to be.
    if count.fetch_add(1, Ordering::Relaxed) >= 1 {
        if kind & MTX_RECURSIVE != MTX_RECURSIVE {
            count.fetch_sub(1, Ordering::Relaxed);
            return THRD_BUSY;
        }
    } else {
        owner.store(tid, Ordering::Relaxed);
    }
    THRD_SUCCESS
}

pub unsafe extern "win64" fn mtx_unlock(m: *mut u8) -> i32 {
    let count = field_i32(m, OFF_COUNT);
    if count.load(Ordering::Relaxed) <= 0 {
        return THRD_SUCCESS; // the STL only asserts: unlock of an unowned mutex
    }
    if count.fetch_sub(1, Ordering::Relaxed) == 1 {
        field_i32(m, OFF_TID).store(-1, Ordering::Relaxed);
        raw_unlock(lock_word(m));
    }
    THRD_SUCCESS
}

// ---------------------------------------------------------------- registration

macro_rules! export {
    ($name:literal, $f:expr, $status:ident) => {
        Export {
            module: "msvcp140",
            name: $name,
            addr: $f as *const (),
            status: Status::$status,
        }
    };
}

pub static EXPORTS: &[Export] = &[
    export!("?_Random_device@std@@YAIXZ", random_device, Native),
    export!("?_Syserror_map@std@@YAPEBDH@Z", syserror_map_export, Native),
    export!("?_Winerror_map@std@@YAHH@Z", winerror_map_export, Native),
    export!("?uncaught_exception@std@@YA_NXZ", uncaught_exception, Stub),
    export!("_Query_perf_counter", query_perf_counter, Native),
    export!("_Query_perf_frequency", query_perf_frequency, Native),
    export!("_Xtime_get_ticks", xtime_get_ticks, Native),
    export!("_Mtx_init_in_situ", mtx_init_in_situ, Native),
    export!("_Mtx_destroy_in_situ", mtx_destroy_in_situ, Native),
    export!("_Mtx_lock", mtx_lock, Native),
    export!("_Mtx_unlock", mtx_unlock, Native),
    export!("??4?$_Yarn@D@std@@QEAAAEAV01@PEBD@Z", yarn_assign, Native),
    export!("?_Xbad_alloc@std@@YAXXZ", xbad_alloc, Stub),
    export!("?_Xbad_function_call@std@@YAXXZ", xbad_function_call, Stub),
    export!(
        "?_Xinvalid_argument@std@@YAXPEBD@Z",
        xinvalid_argument,
        Stub
    ),
    export!("?_Xlength_error@std@@YAXPEBD@Z", xlength_error, Stub),
    export!("?_Xout_of_range@std@@YAXPEBD@Z", xout_of_range, Stub),
    export!("?_Xruntime_error@std@@YAXPEBD@Z", xruntime_error, Stub),
    export!("?_Throw_C_error@std@@YAXH@Z", throw_c_error, Stub),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered() {
        for e in EXPORTS {
            let found = crate::lookup("MSVCP140.dll", e.name).expect(e.name);
            assert_eq!(found.addr, e.addr);
        }
    }

    #[test]
    fn tables_sorted_and_mapped() {
        assert!(WIN_ERRTAB.windows(2).all(|w| w[0].0 < w[1].0));
        unsafe {
            assert_eq!(winerror_map_export(2), ENOENT);
            assert_eq!(winerror_map_export(10065), EHOSTUNREACH);
            assert_eq!(winerror_map_export(0), 0);
            assert_eq!(winerror_map_export(-5), 0);
            assert_eq!(winerror_map_export(7), 0);
            assert_eq!(CStr::from_ptr(syserror_map_export(0)), c"success");
            assert_eq!(
                CStr::from_ptr(syserror_map_export(ENOENT)),
                c"no such file or directory"
            );
            assert_eq!(CStr::from_ptr(syserror_map_export(9999)), c"unknown error");
            assert_eq!(CStr::from_ptr(syserror_map_export(-1)), c"unknown error");
        }
    }

    #[test]
    fn clocks_and_random() {
        unsafe {
            assert_eq!(query_perf_frequency(), 10_000_000);
            let a = query_perf_counter();
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert!(query_perf_counter() > a);
            let t = xtime_get_ticks();
            // After 2020-01-01, in 100 ns units since 1970.
            assert!(t > 1_577_836_800 * 10_000_000);
            assert!(!uncaught_exception());
            let v: Vec<u32> = (0..8).map(|_| random_device()).collect();
            assert!(v.iter().any(|&x| x != v[0]));
        }
    }

    #[test]
    fn thrower_messages() {
        assert_eq!(
            exception_text("out_of_range", "bad"),
            "nativ64: C++ exception std::out_of_range: bad"
        );
        assert_eq!(
            exception_text("bad_alloc", ""),
            "nativ64: C++ exception std::bad_alloc"
        );
        assert_eq!(
            c_error_exception(1).unwrap().1,
            "resource unavailable try again"
        );
        assert_eq!(c_error_exception(2), c_error_exception(1));
        assert_eq!(c_error_exception(3).unwrap().1, "device or resource busy");
        assert_eq!(c_error_exception(4).unwrap().1, "invalid argument");
        assert!(c_error_exception(0).is_none());
        assert!(c_error_exception(99).is_none());
    }

    #[test]
    fn yarn_assign_copies_and_frees() {
        unsafe {
            let mut y = YarnChar {
                ptr: std::ptr::null_mut(),
                nul: 0,
            };
            let r = yarn_assign(&mut y, c"Monday".as_ptr());
            assert_eq!(r, &mut y as *mut _);
            assert_eq!(CStr::from_ptr(y.ptr), c"Monday");
            // Self-assignment keeps the string.
            let same = y.ptr;
            yarn_assign(&mut y, same);
            assert_eq!(y.ptr, same);
            assert_eq!(CStr::from_ptr(y.ptr), c"Monday");
            yarn_assign(&mut y, c"x".as_ptr());
            assert_eq!(CStr::from_ptr(y.ptr), c"x");
            // NULL clears.
            yarn_assign(&mut y, std::ptr::null());
            assert!(y.ptr.is_null());
            let _ = y.nul;
        }
    }

    fn storage() -> Box<[u64; MTX_SIZE / 8]> {
        Box::new([0xAA; MTX_SIZE / 8])
    }

    #[test]
    fn recursive_and_plain_relock() {
        unsafe {
            for kind in [MTX_PLAIN, MTX_PLAIN | MTX_RECURSIVE, 2 | MTX_RECURSIVE] {
                let mut s = storage();
                let m = s.as_mut_ptr() as *mut u8;
                mtx_init_in_situ(m, kind);
                assert_eq!(mtx_lock(m), THRD_SUCCESS);
                assert_eq!(mtx_lock(m), THRD_SUCCESS, "kind {kind:#x}");
                assert_eq!(mtx_unlock(m), THRD_SUCCESS);
                assert_eq!(mtx_unlock(m), THRD_SUCCESS);
                // Fully released: another thread can take it.
                let a = m as usize;
                std::thread::spawn(move || {
                    let m = a as *mut u8;
                    assert_eq!(mtx_lock(m), THRD_SUCCESS);
                    assert_eq!(mtx_unlock(m), THRD_SUCCESS);
                })
                .join()
                .unwrap();
                mtx_destroy_in_situ(m);
            }
            // Non-recursive try/timed mutex: relock is Busy and leaves the count alone.
            for kind in [2, 4, 6] {
                let mut s = storage();
                let m = s.as_mut_ptr() as *mut u8;
                mtx_init_in_situ(m, kind);
                assert_eq!(mtx_lock(m), THRD_SUCCESS);
                assert_eq!(mtx_lock(m), THRD_BUSY, "kind {kind:#x}");
                assert_eq!(mtx_unlock(m), THRD_SUCCESS);
                assert_eq!(mtx_lock(m), THRD_SUCCESS);
                assert_eq!(mtx_unlock(m), THRD_SUCCESS);
            }
            // Unlock of an unowned mutex is a no-op, not an underflow.
            let mut s = storage();
            let m = s.as_mut_ptr() as *mut u8;
            mtx_init_in_situ(m, MTX_PLAIN);
            assert_eq!(mtx_unlock(m), THRD_SUCCESS);
            assert_eq!(mtx_lock(m), THRD_SUCCESS);
            assert_eq!(mtx_unlock(m), THRD_SUCCESS);
        }
    }

    #[test]
    fn mutex_excludes_threads() {
        unsafe {
            let mut s = storage();
            let m = s.as_mut_ptr() as *mut u8;
            mtx_init_in_situ(m, MTX_PLAIN);
            let counter = std::sync::atomic::AtomicU64::new(0);
            // Non-atomic read-modify-write under the mutex: lost updates would show.
            let cell = std::cell::UnsafeCell::new(0u64);
            struct Shared(*mut u8, *const std::cell::UnsafeCell<u64>);
            unsafe impl Send for Shared {}
            unsafe impl Sync for Shared {}
            let sh = Shared(m, &cell);
            std::thread::scope(|sc| {
                for _ in 0..4 {
                    sc.spawn(|| {
                        let sh = &sh;
                        for _ in 0..5000 {
                            mtx_lock(sh.0);
                            mtx_lock(sh.0);
                            *(*sh.1).get() += 1;
                            mtx_unlock(sh.0);
                            mtx_unlock(sh.0);
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });
            assert_eq!(*cell.get(), 20000);
            assert_eq!(counter.load(Ordering::Relaxed), 20000);
        }
    }
}
