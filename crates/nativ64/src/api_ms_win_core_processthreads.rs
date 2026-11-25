use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::handles::{self, ThreadStart, ERROR_INVALID_PARAMETER};
use crate::registry::{Export, Status};
use crate::slots::{SlotTable, Values};
use std::cell::RefCell;
use std::ffi::c_void;

pub const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
/// 64 slots plus 1024 expansion slots, as Windows.
const MAX_TLS: usize = 1088;

static TLS: SlotTable = SlotTable::new(MAX_TLS);

thread_local! {
    static TLS_VALUES: RefCell<Values> = const { RefCell::new(Values::new(&TLS)) };
}

/// `SetThreadAffinityMask` is also imported from `api-ms-win-core-processtopology-obsolete`.
pub static ALIASES: &[Export] = &[Export {
    module: "api-ms-win-core-processtopology-obsolete",
    name: "SetThreadAffinityMask",
    addr: SetThreadAffinityMask as *const (),
    status: Status::Native,
}];

win32! {
    module "api-ms-win-core-processthreads" => "kernel32";
    Native fn GetCurrentProcessId() -> u32 {
        libc::getpid() as u32
    }
    Native fn GetCurrentThreadId() -> u32 {
        libc::syscall(libc::SYS_gettid) as u32
    }
    // Security attributes and inheritance are not modelled.
    Native fn CreateThread(_attrs: *const c_void, stack: usize, start: Option<ThreadStart>,
                           param: *mut c_void, flags: u32, id: *mut u32) -> usize {
        match handles::create_thread(stack, start, param, flags) {
            Some((h, tid)) => {
                if !id.is_null() {
                    *id = tid;
                }
                h
            }
            None => 0,
        }
    }
    Native fn GetCurrentThread() -> usize { handles::CURRENT_THREAD }
    Native fn GetExitCodeThread(h: usize, code: *mut u32) -> i32 {
        if code.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        match handles::get_exit_code(h) {
            Some(c) => {
                *code = c;
                1
            }
            None => 0,
        }
    }
    Native fn OpenThread(_access: u32, _inherit: i32, id: u32) -> usize { handles::open_thread(id) }
    Native fn ResumeThread(h: usize) -> u32 { handles::resume_thread(h) }
    // Only threads that have not entered their start routine can be suspended.
    Native fn SuspendThread(h: usize) -> u32 { handles::suspend_thread(h) }
    Native fn SetThreadPriority(h: usize, p: i32) -> i32 { handles::set_priority(h, p) }
    Native fn GetThreadPriority(h: usize) -> i32 { handles::get_priority(h) }
    Native fn SetThreadAffinityMask(h: usize, mask: usize) -> usize { handles::set_affinity(h, mask) }
    Native fn SwitchToThread() -> i32 { handles::switch_to_thread() }
    // TLS: per-thread slots, 1088 at most; a freed slot reads as NULL in every thread.
    Native fn TlsAlloc() -> u32 {
        match TLS.alloc(0) {
            Some(i) => i,
            None => {
                set_last_error(ERROR_NOT_ENOUGH_MEMORY);
                TLS_OUT_OF_INDEXES
            }
        }
    }
    Native fn TlsFree(index: u32) -> i32 {
        if TLS.free(index, &TLS_VALUES) {
            1
        } else {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
    Native fn TlsGetValue(index: u32) -> *mut c_void {
        match TLS.get(index, &TLS_VALUES) {
            Some(v) => {
                set_last_error(0);
                v as *mut c_void
            }
            None => {
                set_last_error(ERROR_INVALID_PARAMETER);
                std::ptr::null_mut()
            }
        }
    }
    Native fn TlsSetValue(index: u32, value: *mut c_void) -> i32 {
        if TLS.set(index, value as usize, &TLS_VALUES) {
            1
        } else {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        unsafe {
            assert_eq!(GetCurrentProcessId(), std::process::id());
            let main = GetCurrentThreadId();
            assert_ne!(main, 0);
            let other = std::thread::spawn(|| GetCurrentThreadId()).join().unwrap();
            assert_ne!(main, other);
        }
    }

    #[test]
    fn tls_roundtrip_and_errors() {
        use crate::api_ms_win_core_errorhandling::GetLastError;
        unsafe {
            let i = TlsAlloc();
            assert_ne!(i, TLS_OUT_OF_INDEXES);
            set_last_error(5);
            assert!(TlsGetValue(i).is_null());
            assert_eq!(GetLastError(), 0);
            assert_eq!(TlsSetValue(i, 0x99 as *mut _), 1);
            assert_eq!(TlsGetValue(i) as usize, 0x99);
            std::thread::spawn(move || assert!(TlsGetValue(i).is_null()))
                .join()
                .unwrap();
            assert_eq!(TlsFree(i), 1);
            for bad in [i, MAX_TLS as u32, TLS_OUT_OF_INDEXES] {
                set_last_error(0);
                assert!(TlsGetValue(bad).is_null());
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
                set_last_error(0);
                assert_eq!(TlsSetValue(bad, std::ptr::dangling_mut()), 0);
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
                set_last_error(0);
                assert_eq!(TlsFree(bad), 0);
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            }
            // Reallocated index starts NULL.
            let a = TlsAlloc();
            TlsSetValue(a, 3 as *mut _);
            TlsFree(a);
            let b = TlsAlloc();
            assert_eq!(a, b);
            assert!(TlsGetValue(b).is_null());
            TlsFree(b);
        }
    }

    #[test]
    fn affinity_alias_resolves() {
        let a = crate::lookup(
            "api-ms-win-core-processtopology-obsolete-l1-1-0.dll",
            "SetThreadAffinityMask",
        )
        .unwrap();
        let b = crate::lookup("kernel32", "SetThreadAffinityMask").unwrap();
        assert_eq!(a.addr, b.addr);
    }
}
