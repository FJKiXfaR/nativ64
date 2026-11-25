use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The process-wide top-level exception filter (0: none), called by `UnhandledExceptionFilter`.
static UNHANDLED_FILTER: AtomicUsize = AtomicUsize::new(0);

/// Vectored exception handlers `(handle, handler)` in call order (first-registered `first`
/// ones at the front); [`crate::seh`] calls them before the frames' handlers.
static VECTORED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
static NEXT_VECTORED: AtomicUsize = AtomicUsize::new(1);

/// The MSVC convention for naming a thread: `RaiseException` with this code and a
/// `THREADNAME_INFO { u32 kind = 0x1000; const char *name; u32 thread_id (-1: caller); u32 flags }`
/// as its arguments, caught by the debugger or the caller's own handler.
pub(crate) const THREAD_NAME_CODE: u32 = 0x406D_1388;

/// Names the calling thread as `RaiseException(THREAD_NAME_CODE)` asks; returns false when the
/// arguments are not a thread-name request. Other threads' names are dropped (shortcut).
pub(crate) unsafe fn name_thread(nargs: u32, args: *const usize) -> bool {
    if args.is_null() || nargs < 3 {
        return false;
    }
    let info = args.cast::<u8>();
    if info.cast::<u32>().read_unaligned() != 0x1000 {
        return false;
    }
    let name = info.add(8).cast::<*const libc::c_char>().read_unaligned();
    let tid = info.add(16).cast::<u32>().read_unaligned();
    let me = crate::api_ms_win_core_processthreads::GetCurrentThreadId();
    if !name.is_null() && (tid == u32::MAX || tid == me) {
        // Linux keeps 15 bytes and a NUL.
        let mut buf = [0 as libc::c_char; 16];
        let mut i = 0;
        while i < 15 && *name.add(i) != 0 {
            buf[i] = *name.add(i);
            i += 1;
        }
        libc::pthread_setname_np(libc::pthread_self(), buf.as_ptr());
    }
    true
}

/// The registered vectored handlers, in call order.
pub fn vectored_handlers() -> Vec<usize> {
    let list = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().map(|&(_, h)| h).collect()
}

/// The filter `SetUnhandledExceptionFilter` installed (0: none).
pub fn top_level_filter() -> usize {
    UNHANDLED_FILTER.load(Ordering::Acquire)
}

/// `RaiseException`, which must capture its caller's context and so is assembly
/// ([`crate::seh::nativ64_raise_exception`]): dispatches the exception to the vectored handlers
/// and the frames' handlers; returns when one continues execution, or (thread-name request)
/// when nobody took it.
pub static ASM_EXPORTS: &[crate::Export] = &[
    crate::Export {
        module: "api-ms-win-core-errorhandling",
        name: "RaiseException",
        addr: crate::seh::nativ64_raise_exception as *const (),
        status: crate::Status::Native,
    },
    crate::Export {
        module: "kernel32",
        name: "RaiseException",
        addr: crate::seh::nativ64_raise_exception as *const (),
        status: crate::Status::Native,
    },
];

thread_local! {
    static LAST_ERROR: Cell<u32> = const { Cell::new(0) };
}

/// Used by other modules to report failures the Windows way.
pub fn set_last_error(code: u32) {
    LAST_ERROR.with(|e| e.set(code));
}

win32! {
    module "api-ms-win-core-errorhandling" => "kernel32";
    Native fn GetLastError() -> u32 {
        LAST_ERROR.with(|e| e.get())
    }
    Native fn SetLastError(code: u32) {
        set_last_error(code)
    }
    // Installs the filter and returns the previous one (Windows semantics).
    Native fn SetUnhandledExceptionFilter(filter: usize) -> usize {
        UNHANDLED_FILTER.swap(filter, Ordering::AcqRel)
    }
    // Returns an opaque non-zero handle (an index, not an entry pointer as on Windows); a NULL
    // handler is refused with 0.
    Native fn AddVectoredExceptionHandler(first: u32, handler: usize) -> usize {
        if handler == 0 {
            return 0;
        }
        let handle = NEXT_VECTORED.fetch_add(1, Ordering::Relaxed);
        let mut list = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
        if first != 0 {
            list.insert(0, (handle, handler));
        } else {
            list.push((handle, handler));
        }
        handle
    }
    // Verdict of the title's top-level filter (-1 continue, 1 execute handler); without one, or
    // when it declines, reports the exception and stops.
    Native fn UnhandledExceptionFilter(info: *const crate::seh::ExceptionPointers) -> i32 {
        crate::seh::unhandled_exception_filter(info)
    }
    Native fn RemoveVectoredExceptionHandler(handle: usize) -> u32 {
        let mut list = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
        match list.iter().position(|&(h, _)| h == handle) {
            Some(i) => {
                list.remove(i);
                1
            }
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seh::nativ64_raise_exception as RaiseException;

    unsafe extern "win64" fn no_filter(_: *const crate::seh::ExceptionPointers) -> i32 {
        0
    }
    unsafe extern "win64" fn no_vectored(_: *mut crate::seh::ExceptionPointers) -> i32 {
        0
    }

    #[test]
    fn per_thread() {
        unsafe {
            SetLastError(5);
            assert_eq!(GetLastError(), 5);
            std::thread::spawn(|| {
                assert_eq!(GetLastError(), 0);
                SetLastError(7);
            })
            .join()
            .unwrap();
            assert_eq!(GetLastError(), 5);
        }
    }

    #[test]
    fn vectored_handlers_add_and_remove() {
        unsafe {
            assert_eq!(AddVectoredExceptionHandler(1, 0), 0);
            let a = AddVectoredExceptionHandler(0, no_vectored as *const () as usize);
            let b = AddVectoredExceptionHandler(1, no_vectored as *const () as usize);
            assert!(a != 0 && b != 0 && a != b);
            {
                let list = VECTORED.lock().unwrap();
                let pa = list.iter().position(|e| e.0 == a).unwrap();
                let pb = list.iter().position(|e| e.0 == b).unwrap();
                assert!(pb < pa, "a first handler goes before the others");
            }
            assert_eq!(RemoveVectoredExceptionHandler(a), 1);
            assert_eq!(RemoveVectoredExceptionHandler(a), 0);
            assert_eq!(RemoveVectoredExceptionHandler(b), 1);
        }
    }

    #[test]
    fn raise_exception_names_the_thread() {
        std::thread::spawn(|| unsafe {
            let name = c"a-long-thread-name-here";
            let info: [usize; 3] = [0x1000, name.as_ptr() as usize, u32::MAX as usize];
            RaiseException(THREAD_NAME_CODE, 0, 3, info.as_ptr());
            let mut buf = [0 as libc::c_char; 32];
            libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr(), buf.len());
            let got = std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
            assert_eq!(got, "a-long-thread-n");
            assert!(!name_thread(3, [0x2000usize, 0, 0].as_ptr()));
            assert!(!name_thread(1, info.as_ptr()));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn unhandled_filter_returns_previous() {
        let _lock = crate::seh::FILTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            let (f1, f2) = (
                no_filter as *const () as usize,
                no_vectored as *const () as usize,
            );
            let first = SetUnhandledExceptionFilter(f1);
            assert_eq!(SetUnhandledExceptionFilter(f2), f1);
            SetUnhandledExceptionFilter(first);
        }
    }
}
