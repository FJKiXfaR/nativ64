use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The process-wide top-level exception filter (0: none). Nothing dispatches exceptions to it yet.
static UNHANDLED_FILTER: AtomicUsize = AtomicUsize::new(0);

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unhandled_filter_returns_previous() {
        unsafe {
            let first = SetUnhandledExceptionFilter(0x1000);
            assert_eq!(SetUnhandledExceptionFilter(0x2000), 0x1000);
            SetUnhandledExceptionFilter(first);
        }
    }
}
