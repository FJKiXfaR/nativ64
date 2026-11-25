use crate::api_ms_win_core_errorhandling::set_last_error;
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

/// Writes the current time as a SYSTEMTIME (eight little-endian WORDs), in UTC or local time.
unsafe fn store_systemtime(out: *mut u16, local: bool) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    let mut tm: libc::tm = std::mem::zeroed();
    if local {
        libc::localtime_r(&ts.tv_sec, &mut tm);
    } else {
        libc::gmtime_r(&ts.tv_sec, &mut tm);
    }
    let f = [
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_wday,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec.min(59),
        (ts.tv_nsec / 1_000_000) as i32,
    ];
    for (i, v) in f.iter().enumerate() {
        out.add(i).write_unaligned(*v as u16);
    }
}

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_INVALID_PARAMETER: u32 = 87;
/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION`: mask, relationship, 16 bytes of union.
const LPI_SIZE: usize = 32;
const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;

/// The logical processors this process may run on, as a mask (at most 64: one processor group).
fn processor_mask() -> u64 {
    // SAFETY: a zeroed cpu_set_t is valid, and the call fills it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } == 0;
    let mut mask = 0u64;
    if ok {
        for i in 0..64 {
            // SAFETY: `i` is below CPU_SETSIZE.
            if unsafe { libc::CPU_ISSET(i, &set) } {
                mask |= 1 << i;
            }
        }
    }
    if mask == 0 {
        1
    } else {
        mask
    }
}

/// One core entry per logical processor (no SMT siblings reported), then one package entry.
unsafe fn logical_processor_information(buffer: *mut u8, length: *mut u32) -> i32 {
    if length.is_null() {
        set_last_error(87);
        return 0;
    }
    let mask = processor_mask();
    let entries = mask.count_ones() as usize + 1;
    let need = entries * LPI_SIZE;
    if (length.read_unaligned() as usize) < need || buffer.is_null() {
        length.write_unaligned(need as u32);
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    let mut out = buffer;
    let mut put = |mask: u64, relation: u32| {
        std::ptr::write_bytes(out, 0, LPI_SIZE);
        (out as *mut u64).write_unaligned(mask);
        (out.add(8) as *mut u32).write_unaligned(relation);
        out = out.add(LPI_SIZE);
    };
    for bit in (0..64).filter(|b| mask >> b & 1 != 0) {
        put(1 << bit, RELATION_PROCESSOR_CORE);
    }
    put(mask, RELATION_PROCESSOR_PACKAGE);
    length.write_unaligned(need as u32);
    1
}

win32! {
    module "api-ms-win-core-sysinfo" => "kernel32";
    Native fn GetLogicalProcessorInformation(buffer: *mut u8, length: *mut u32) -> i32 {
        logical_processor_information(buffer, length)
    }
    Native fn GetSystemTimeAsFileTime(out: *mut u32) {
        store_filetime(out, filetime_now());
    }
    // The clock already has nanosecond resolution, so the precise variant is the same.
    Native fn GetSystemTimePreciseAsFileTime(out: *mut u32) {
        store_filetime(out, filetime_now());
    }
    Native fn GetSystemTime(out: *mut u16) {
        store_systemtime(out, false);
    }
    Native fn GetLocalTime(out: *mut u16) {
        store_systemtime(out, true);
    }
    // MEMORYSTATUSEX (64 bytes): the host's RAM and swap; virtual space is the user half (128 TiB).
    Native fn GlobalMemoryStatusEx(out: *mut u8) -> i32 {
        if out.is_null() || (out as *const u32).read_unaligned() != 64 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let mut si: libc::sysinfo = std::mem::zeroed();
        libc::sysinfo(&mut si);
        let unit = si.mem_unit.max(1) as u64;
        let (total, avail) = (si.totalram as u64 * unit, si.freeram as u64 * unit);
        let (swap, swap_free) = (si.totalswap as u64 * unit, si.freeswap as u64 * unit);
        let virt = 0x7FFF_FFFE_FFFFu64;
        let load = (100 - (avail * 100).checked_div(total).unwrap_or(100)) as u32;
        let f = [total, avail, total + swap, avail + swap_free, virt, virt, 0];
        out.add(4).cast::<u32>().write_unaligned(load);
        for (i, v) in f.iter().enumerate() {
            out.add(8 + 8 * i).cast::<u64>().write_unaligned(*v);
        }
        1
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
    fn global_memory_status() {
        let mut b = [0u8; 64];
        unsafe {
            assert_eq!(GlobalMemoryStatusEx(b.as_mut_ptr()), 0);
            assert_eq!(GlobalMemoryStatusEx(std::ptr::null_mut()), 0);
            b[0] = 64;
            assert_eq!(GlobalMemoryStatusEx(b.as_mut_ptr()), 1);
        }
        let total = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let avail = u64::from_le_bytes(b[16..24].try_into().unwrap());
        assert!(total > 0 && avail <= total);
        assert!(b[4] <= 100);
    }

    #[test]
    fn systemtime_is_current_and_well_formed() {
        let mut utc = [0u16; 8];
        let mut local = [0u16; 8];
        unsafe {
            GetSystemTime(utc.as_mut_ptr());
            GetLocalTime(local.as_mut_ptr());
        }
        let year = 1970 + std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 31_556_952;
        for t in [utc, local] {
            assert!((t[0] as u64).abs_diff(year) <= 1, "{t:?}");
            assert!((1..=12).contains(&t[1]) && t[2] <= 6 && (1..=31).contains(&t[3]));
            assert!(t[4] < 24 && t[5] < 60 && t[6] < 60 && t[7] < 1000);
        }
    }

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

    #[test]
    fn logical_processors() {
        unsafe {
            let mut len = 0u32;
            assert_eq!(
                GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len),
                0
            );
            assert_eq!(crate::api_ms_win_core_errorhandling::GetLastError(), 122);
            assert!(len >= 64 && len.is_multiple_of(32));
            let mut buf = vec![0u8; len as usize];
            assert_eq!(
                GetLogicalProcessorInformation(buf.as_mut_ptr(), &mut len),
                1
            );
            let last = &buf[len as usize - 32..];
            assert_eq!(u32::from_le_bytes(last[8..12].try_into().unwrap()), 3);
            let all = u64::from_le_bytes(last[..8].try_into().unwrap());
            assert_eq!(all.count_ones() as usize, len as usize / 32 - 1);
            assert_eq!(
                GetLogicalProcessorInformation(buf.as_mut_ptr(), std::ptr::null_mut()),
                0
            );
        }
    }
}
