use std::cell::Cell;

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
}
