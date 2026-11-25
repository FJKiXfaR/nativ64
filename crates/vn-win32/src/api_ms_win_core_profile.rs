/// Same tick rate as Windows: 10 MHz.
const FREQUENCY: i64 = 10_000_000;

pub(crate) fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

win32! {
    module "api-ms-win-core-profile" => "kernel32";
    Native fn QueryPerformanceCounter(count: *mut i64) -> i32 {
        if count.is_null() {
            crate::api_ms_win_core_errorhandling::set_last_error(998); // ERROR_NOACCESS
            return 0;
        }
        *count = (monotonic_ns() / 100) as i64;
        1
    }
    Native fn QueryPerformanceFrequency(freq: *mut i64) -> i32 {
        if freq.is_null() {
            crate::api_ms_win_core_errorhandling::set_last_error(998);
            return 0;
        }
        *freq = FREQUENCY;
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_advances() {
        unsafe {
            let (mut f, mut a, mut b) = (0i64, 0i64, 0i64);
            assert_eq!(QueryPerformanceFrequency(&mut f), 1);
            assert_eq!(f, 10_000_000);
            assert_eq!(QueryPerformanceCounter(&mut a), 1);
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(QueryPerformanceCounter(&mut b), 1);
            assert!(b - a >= f / 50 - f / 100);
            assert_eq!(QueryPerformanceCounter(std::ptr::null_mut()), 0);
            assert_eq!(crate::api_ms_win_core_errorhandling::GetLastError(), 998);
        }
    }
}
