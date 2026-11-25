//! `api-ms-win-core-fibers`: fiber local storage (FLS).
//!
//! Fibers themselves are not implemented, so FLS is per thread. At most 4080 slots (as Windows).
//! `FlsFree` runs the callback for the calling thread's non-NULL value only; callbacks also run
//! at thread exit for the values still set. Errors: `FLS_OUT_OF_INDEXES` with
//! `ERROR_NOT_ENOUGH_MEMORY` when the table is full, `ERROR_INVALID_PARAMETER` for an index that
//! is not allocated. `FlsGetValue` clears the last error on success.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::handles::ERROR_INVALID_PARAMETER;
use crate::slots::{SlotTable, Values};
use std::cell::RefCell;
use std::ffi::c_void;

pub const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const MAX_FLS: usize = 4080;

static TABLE: SlotTable = SlotTable::new(MAX_FLS);

thread_local! {
    static VALUES: RefCell<Values> = const { RefCell::new(Values::new(&TABLE)) };
}

win32! {
    module "api-ms-win-core-fibers" => "kernel32";
    Native fn FlsAlloc(callback: usize) -> u32 {
        match TABLE.alloc(callback) {
            Some(i) => i,
            None => {
                set_last_error(ERROR_NOT_ENOUGH_MEMORY);
                FLS_OUT_OF_INDEXES
            }
        }
    }
    Native fn FlsFree(index: u32) -> i32 {
        if TABLE.free(index, &VALUES) {
            1
        } else {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
    Native fn FlsGetValue(index: u32) -> *mut c_void {
        match TABLE.get(index, &VALUES) {
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
    Native fn FlsSetValue(index: u32, value: *mut c_void) -> i32 {
        if TABLE.set(index, value as usize, &VALUES) {
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
    use crate::api_ms_win_core_errorhandling::GetLastError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The tests share the global table, and allocation takes the lowest free index: a test that
    /// frees an index and then expects it to be invalid would race with another test reusing it.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    static SEEN: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "win64" fn cb(p: *mut c_void) {
        SEEN.fetch_add(p as usize, Ordering::SeqCst);
    }
    static SEEN2: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "win64" fn cb2(p: *mut c_void) {
        SEEN2.fetch_add(p as usize, Ordering::SeqCst);
    }

    #[test]
    fn basic_and_errors() {
        let _g = serial();
        unsafe {
            let i = FlsAlloc(0);
            assert_ne!(i, FLS_OUT_OF_INDEXES);
            set_last_error(5);
            assert!(FlsGetValue(i).is_null());
            assert_eq!(GetLastError(), 0);
            assert_eq!(FlsSetValue(i, 0x1234 as *mut _), 1);
            assert_eq!(FlsGetValue(i) as usize, 0x1234);
            // Another thread sees NULL.
            std::thread::spawn(move || assert!(FlsGetValue(i).is_null()))
                .join()
                .unwrap();
            assert_eq!(FlsFree(i), 1);
            // Freed and out-of-range indices.
            for bad in [i, 4080, 0xFFFF_FFFF] {
                set_last_error(0);
                assert!(FlsGetValue(bad).is_null());
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
                set_last_error(0);
                assert_eq!(FlsSetValue(bad, std::ptr::dangling_mut()), 0);
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
                set_last_error(0);
                assert_eq!(FlsFree(bad), 0);
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            }
        }
    }

    #[test]
    fn free_runs_callback_and_reuse_is_null() {
        let _g = serial();
        unsafe {
            let i = FlsAlloc(cb as *const () as usize);
            assert_eq!(FlsSetValue(i, 5 as *mut _), 1);
            assert_eq!(FlsFree(i), 1);
            assert_eq!(SEEN.load(Ordering::SeqCst), 5);
            let j = FlsAlloc(0);
            // j may differ from i; slots.rs pins reuse.
            assert!(FlsGetValue(j).is_null());
            FlsFree(j);
            // NULL value: no callback.
            let k = FlsAlloc(cb as *const () as usize);
            FlsFree(k);
            assert_eq!(SEEN.load(Ordering::SeqCst), 5);
        }
    }

    #[test]
    fn thread_exit_runs_callbacks() {
        let _g = serial();
        unsafe {
            let i = FlsAlloc(cb2 as *const () as usize);
            let z = FlsAlloc(cb2 as *const () as usize); // stays NULL
            let t = std::thread::spawn(move || {
                assert_eq!(FlsSetValue(i, 7 as *mut _), 1);
                let _ = z;
            });
            t.join().unwrap();
            assert_eq!(SEEN2.load(Ordering::SeqCst), 7);
            FlsFree(i);
            FlsFree(z);
        }
    }
}
