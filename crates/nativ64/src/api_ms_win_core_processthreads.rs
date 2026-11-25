use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::handles::{self, ThreadStart, ERROR_INVALID_PARAMETER};
use std::ffi::c_void;

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
}
