use crate::api_ms_win_core_profile::monotonic_ns;

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

/// The current UTC time as a FILETIME value (100 ns intervals since 1601).
fn filetime_now() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    UNIX_EPOCH_AS_FILETIME + ts.tv_sec as u64 * 10_000_000 + ts.tv_nsec as u64 / 100
}

/// Writes a FILETIME (two little-endian DWORDs, 4-byte aligned) through `out`.
unsafe fn store_filetime(out: *mut u32, t: u64) {
    out.write_unaligned(t as u32);
    out.add(1).write_unaligned((t >> 32) as u32);
}

win32! {
    module "api-ms-win-core-sysinfo" => "kernel32";
    Native fn GetSystemTimeAsFileTime(out: *mut u32) {
        store_filetime(out, filetime_now());
    }
    // The clock already has nanosecond resolution, so the precise variant is the same.
    Native fn GetSystemTimePreciseAsFileTime(out: *mut u32) {
        store_filetime(out, filetime_now());
    }
    Native fn GetTickCount() -> u32 {
        (monotonic_ns() / 1_000_000) as u32
    }
    Native fn GetTickCount64() -> u64 {
        monotonic_ns() / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_is_current_utc() {
        for f in [
            GetSystemTimeAsFileTime as unsafe extern "win64" fn(*mut u32),
            GetSystemTimePreciseAsFileTime,
        ] {
            let mut ft = [0u32; 2];
            unsafe { f(ft.as_mut_ptr()) };
            let v = ft[0] as u64 | (ft[1] as u64) << 32;
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                / 100;
            let expect = UNIX_EPOCH_AS_FILETIME + unix;
            assert!(expect.abs_diff(v) < 10_000_000, "{v} vs {expect}");
        }
    }

    #[test]
    fn filetime_advances() {
        let mut a = [0u32; 2];
        let mut b = [0u32; 2];
        unsafe {
            GetSystemTimePreciseAsFileTime(a.as_mut_ptr());
            std::thread::sleep(std::time::Duration::from_millis(20));
            GetSystemTimePreciseAsFileTime(b.as_mut_ptr());
        }
        let (a, b) = (
            a[0] as u64 | (a[1] as u64) << 32,
            b[0] as u64 | (b[1] as u64) << 32,
        );
        assert!(b - a >= 150_000);
    }

    #[test]
    fn ticks() {
        unsafe {
            let a = GetTickCount64();
            std::thread::sleep(std::time::Duration::from_millis(30));
            let b = GetTickCount64();
            assert!(b - a >= 25);
            assert!(GetTickCount().abs_diff(b as u32) < 1000);
        }
    }
}
