//! `api-ms-win-core-timezone`: `FILETIME` <-> `SYSTEMTIME` conversions.
//!
//! Windows' `SYSTEMTIME` is eight `u16` (year, month, day of week, day, hour, minute, second,
//! millisecond); `FILETIME` is the 100 ns count since 1601 UTC. The conversion goes through the
//! host's C library (see `filetime.rs`), so the calendar and its range are the host's as well
//! (Windows' own years run 1601 through 30827). A date that does not exist in the calendar
//! (February 30, month 13) fails like Windows does, without writing the output.

use crate::filetime::{self, SYSTEMTIME_SIZE};

const ERROR_INVALID_PARAMETER: u32 = 87;

/// Copies a `SYSTEMTIME` (eight little-endian `u16`) from a caller buffer.
unsafe fn read_systemtime(st: *const u16) -> [u16; SYSTEMTIME_SIZE / 2] {
    let mut out = [0u16; SYSTEMTIME_SIZE / 2];
    for (i, v) in out.iter_mut().enumerate() {
        *v = unsafe { st.add(i).read_unaligned() };
    }
    out
}

/// Writes a `SYSTEMTIME` (eight little-endian `u16`) to a caller buffer.
unsafe fn write_systemtime(out: *mut u16, st: &[u16; SYSTEMTIME_SIZE / 2]) {
    for (i, v) in st.iter().enumerate() {
        unsafe { out.add(i).write_unaligned(*v) };
    }
}

unsafe fn filetime_to_systemtime(ft: *const u64, st: *mut u16) -> i32 {
    if ft.is_null() || st.is_null() {
        crate::api_ms_win_core_errorhandling::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let value = unsafe { ft.read_unaligned() };
    let Some(sys) = filetime::to_systemtime(value) else {
        return 0;
    };
    unsafe { write_systemtime(st, &sys) };
    1
}

unsafe fn systemtime_to_filetime(st: *const u16, ft: *mut u64) -> i32 {
    if st.is_null() || ft.is_null() {
        crate::api_ms_win_core_errorhandling::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let sys = unsafe { read_systemtime(st) };
    let Some(secs) = filetime::from_systemtime(&sys) else {
        return 0;
    };
    // The millisecond field keeps its low 3 digits (as Windows does).
    let value = filetime::from_parts(secs, u32::from(sys[7]) * 10_000);
    unsafe { ft.write_unaligned(value) };
    1
}

win32! {
    module "api-ms-win-core-timezone" => "kernel32";
    Native fn FileTimeToSystemTime(ft: *const u64, st: *mut u16) -> i32 {
        unsafe { filetime_to_systemtime(ft, st) }
    }
    Native fn SystemTimeToFileTime(st: *const u16, ft: *mut u64) -> i32 {
        unsafe { systemtime_to_filetime(st, ft) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;

    #[test]
    fn registered_both_names() {
        for name in ["FileTimeToSystemTime", "SystemTimeToFileTime"] {
            assert!(
                lookup("api-ms-win-core-timezone-l1-1-0.dll", name).is_some(),
                "{name}"
            );
            assert!(lookup("kernel32.dll", name).is_some(), "{name}");
        }
    }

    #[test]
    fn epoch_round_trips_through_systemtime() {
        let epoch = filetime::UNIX_EPOCH_AS_FILETIME;
        let mut st = [0u16; 8];
        let mut back = 0u64;
        unsafe {
            assert_eq!(FileTimeToSystemTime(&epoch, st.as_mut_ptr()), 1);
            assert_eq!(st, [1970, 1, 4, 1, 0, 0, 0, 0]);
            assert_eq!(SystemTimeToFileTime(st.as_ptr(), &mut back), 1);
            assert_eq!(back, epoch);
        }
    }

    #[test]
    fn milliseconds_and_invalid_dates() {
        let mut st = [2020u16, 2, 0, 29, 12, 34, 56, 789];
        let mut ft = 0u64;
        unsafe {
            assert_eq!(SystemTimeToFileTime(st.as_ptr(), &mut ft), 1);
            st[4] = 12;
            assert_eq!(FileTimeToSystemTime(&ft, st.as_mut_ptr()), 1);
            assert_eq!(st, [2020, 2, 6, 29, 12, 34, 56, 789]);
            // February 30 is not a date.
            st = [2021, 2, 0, 30, 0, 0, 0, 0];
            assert_eq!(SystemTimeToFileTime(st.as_ptr(), &mut ft), 0);
            // Out of the Windows range.
            let year1600 = filetime::from_parts(-116_444_736_000 + 1, 0);
            assert_eq!(FileTimeToSystemTime(&year1600, st.as_mut_ptr()), 0);
        }
    }

    #[test]
    fn null_pointers_fail() {
        unsafe {
            assert_eq!(
                FileTimeToSystemTime(std::ptr::null(), std::ptr::null_mut()),
                0
            );
            assert_eq!(
                SystemTimeToFileTime(std::ptr::null(), std::ptr::null_mut()),
                0
            );
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_INVALID_PARAMETER
            );
        }
    }
}
