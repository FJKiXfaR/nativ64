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

/// `SYSTEM_INFO` (48 bytes on x64): the host's processor configuration as Windows reports it.
/// `GetSystemInfo` and `GetNativeSystemInfo` are the same on x64 - the process is never WOW64.
unsafe fn system_info(out: *mut u8) {
    if out.is_null() {
        return;
    }
    let mask = processor_mask();
    let pages = libc::sysconf(libc::_SC_PAGESIZE);
    let (level, revision) = processor_level_and_revision();
    std::ptr::write_bytes(out, 0, 48);
    // The union: wProcessorArchitecture (PROCESSOR_ARCHITECTURE_AMD64 = 9), wReserved = 0.
    (out as *mut u16).write_unaligned(9);
    (out.add(4) as *mut u32).write_unaligned(if pages > 0 { pages as u32 } else { 4096 });
    (out.add(8) as *mut u64).write_unaligned(0x1_0000);
    (out.add(16) as *mut u64).write_unaligned(0x7FFF_FFFE_FFFF);
    (out.add(24) as *mut u64).write_unaligned(mask);
    (out.add(32) as *mut u32).write_unaligned(mask.count_ones());
    (out.add(36) as *mut u32).write_unaligned(0x21D8); // PROCESSOR_AMD_X8664
    (out.add(40) as *mut u32).write_unaligned(0x1_0000);
    (out.add(44) as *mut u16).write_unaligned(level);
    (out.add(46) as *mut u16).write_unaligned(revision);
}

/// The processor level (family) and revision (model << 8 | stepping) Windows fills
/// `SYSTEM_INFO` with, from CPUID leaf 1.
fn processor_level_and_revision() -> (u16, u16) {
    let r = std::arch::x86_64::__cpuid(1);
    let base_family = ((r.eax >> 8) & 0xf) as u16;
    let base_model = ((r.eax >> 4) & 0xf) as u16;
    let ext_family = ((r.eax >> 20) & 0xff) as u16;
    let ext_model = ((r.eax >> 16) & 0xf) as u16;
    let family = if base_family == 0xf {
        base_family + ext_family
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | ext_model << 4
    } else {
        base_model
    };
    (family, model << 8 | (r.eax & 0xf) as u16)
}

/// `GetSystemTimeAdjustment`: the host clock's tick in 100 ns units, with periodic adjustment
/// disabled (the same shape Windows reports; its own default tick is 15.625 ms).
unsafe fn system_time_adjustment(
    adjustment: *mut u32,
    increment: *mut u32,
    disabled: *mut i32,
) -> i32 {
    if adjustment.is_null() || increment.is_null() || disabled.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut res = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ok = libc::clock_getres(libc::CLOCK_MONOTONIC, &mut res) == 0;
    let ns = if ok {
        res.tv_sec as u64 * 1_000_000_000 + res.tv_nsec as u64
    } else {
        0
    };
    let tick = if ns == 0 {
        156_250
    } else {
        ns.div_ceil(100).clamp(1, u64::from(u32::MAX)) as u32
    };
    adjustment.write_unaligned(tick);
    increment.write_unaligned(tick);
    disabled.write_unaligned(1);
    1
}

const RELATION_ALL: u32 = 0xffff;
/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` (8-byte header) + `PROCESSOR_RELATIONSHIP`
/// (24-byte body + one 16-byte `GROUP_AFFINITY`).
const LPI2_ENTRY: usize = 8 + 24 + 16;

/// One `PROCESSOR_RELATIONSHIP` entry per logical processor, then one package entry when the
/// caller asks for it (`GetLogicalProcessorInformationEx`).
unsafe fn logical_processor_information_ex(
    relationship: u32,
    buffer: *mut u8,
    length: *mut u32,
) -> i32 {
    if length.is_null()
        || relationship != RELATION_PROCESSOR_CORE
            && relationship != RELATION_PROCESSOR_PACKAGE
            && relationship != RELATION_ALL
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mask = processor_mask();
    let cores = mask.count_ones() as usize;
    // The relationship is an enum value; `RELATION_ALL` asks for every kind.
    let want = |rel: u32| relationship == rel || relationship == RELATION_ALL;
    let packages = usize::from(want(RELATION_PROCESSOR_PACKAGE));
    let need = (cores + packages) * LPI2_ENTRY;
    if buffer.is_null() || (length.read_unaligned() as usize) < need {
        length.write_unaligned(need as u32);
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    let mut out = buffer;
    let mut put = |mask: u64| {
        // Relationship, then the record's size including the header.
        (out as *mut u32).write_unaligned(RELATION_PROCESSOR_CORE);
        (out.add(4) as *mut u32).write_unaligned(LPI2_ENTRY as u32);
        // PROCESSOR_RELATIONSHIP: Flags, EfficiencyClass, 20 reserved bytes, GroupCount.
        std::ptr::write_bytes(out.add(8), 0, LPI2_ENTRY - 8);
        (out.add(8 + 22) as *mut u16).write_unaligned(1);
        // GROUP_AFFINITY: KAFFINITY mask, WORD Group, WORD Reserved[3].
        (out.add(8 + 24) as *mut u64).write_unaligned(mask);
        out = out.add(LPI2_ENTRY);
    };
    if want(RELATION_PROCESSOR_CORE) {
        for bit in (0..64).filter(|b| mask >> b & 1 != 0) {
            put(1 << bit);
        }
    }
    if want(RELATION_PROCESSOR_PACKAGE) {
        put(mask);
    }
    length.write_unaligned(need as u32);
    1
}

/// `GetSystemTimes`: the host's cumulative times in 100 ns units. Windows' "kernel" time
/// includes the idle time; the idle value is reported separately, as there.
fn system_times(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32 {
    if idle.is_null() || kernel.is_null() || user.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Ok(stat) = std::fs::read_to_string("/proc/stat") else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let Some(line) = stat.lines().next() else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let hz = {
        let h = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if h > 0 {
            h as u64
        } else {
            100
        }
    };
    let ticks = |t: u64| t.saturating_mul(10_000_000) / hz;
    let idle_time = fields[3] + fields.get(4).copied().unwrap_or(0);
    let kernel_time = fields[2];
    let user_time = fields[0] + fields[1];
    unsafe {
        idle.write_unaligned(ticks(idle_time));
        kernel.write_unaligned(ticks(kernel_time + idle_time));
        user.write_unaligned(ticks(user_time));
    }
    1
}

/// The host's RAM and swap for `MEMORYSTATUS` (32-bit fields; values above 4 GiB clamp).
fn memory_status() -> (u32, u64, u64, u64, u64) {
    let mut si: libc::sysinfo = unsafe { std::mem::zeroed() };
    unsafe { libc::sysinfo(&mut si) };
    let unit = si.mem_unit.max(1) as u64;
    let total = si.totalram as u64 * unit;
    let avail = si.freeram as u64 * unit;
    let swap = si.totalswap as u64 * unit;
    let swap_free = si.freeswap as u64 * unit;
    let load = (100 - (avail * 100).checked_div(total).unwrap_or(100)) as u32;
    (load, total, avail, total + swap, avail + swap_free)
}

/// `GetPhysicallyInstalledSystemMemory`: the installed RAM in kilobytes.
unsafe fn physically_installed_memory(out: *mut u64) -> i32 {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut si: libc::sysinfo = unsafe { std::mem::zeroed() };
    unsafe { libc::sysinfo(&mut si) };
    let unit = si.mem_unit.max(1) as u64;
    out.write_unaligned((si.totalram as u64 * unit) / 1024);
    1
}

/// `GetLargePageMinimum`: the host's huge page size, 2 MiB when the kernel names none.
fn large_page_minimum() -> usize {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("Hugepagesize:") {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|k| k.parse::<usize>().ok())
                {
                    if kb > 0 {
                        return kb * 1024;
                    }
                }
            }
        }
    }
    2 << 20
}

win32! {
    module "api-ms-win-core-sysinfo" => "kernel32";
    Native fn GetSystemInfo(out: *mut u8) {
        unsafe { system_info(out) }
    }
    // On x64 the native form is the same structure (`GetSystemInfo` is the WOW64-compatible one).
    Native fn GetNativeSystemInfo(out: *mut u8) {
        unsafe { system_info(out) }
    }
    Native fn GetSystemTimeAdjustment(adjustment: *mut u32, increment: *mut u32, disabled: *mut i32) -> i32 {
        unsafe { system_time_adjustment(adjustment, increment, disabled) }
    }
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
    Native fn GetLogicalProcessorInformationEx(relationship: u32, buffer: *mut u8, length: *mut u32) -> i32 {
        unsafe { logical_processor_information_ex(relationship, buffer, length) }
    }
    Native fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32 {
        system_times(idle, kernel, user)
    }
    // MEMORYSTATUS: the same numbers as the Ex form, in 32-bit fields.
    Native fn GlobalMemoryStatus(out: *mut u8) -> i32 {
        if out.is_null() || unsafe { (out as *const u32).read_unaligned() } != 32 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let (load, total, avail, page, page_avail) = memory_status();
        let clamp = |v: u64| if v > u64::from(u32::MAX) { u32::MAX } else { v as u32 };
        unsafe {
            (out.add(4) as *mut u32).write_unaligned(load);
            (out.add(8) as *mut u32).write_unaligned(clamp(total));
            (out.add(12) as *mut u32).write_unaligned(clamp(avail));
            (out.add(16) as *mut u32).write_unaligned(clamp(page));
            (out.add(20) as *mut u32).write_unaligned(clamp(page_avail));
            // dwTotalVirtual / dwAvailVirtual: the user-half address space, clamped.
            (out.add(24) as *mut u32).write_unaligned(u32::MAX);
            (out.add(28) as *mut u32).write_unaligned(u32::MAX);
        }
        1
    }
    Native fn GetPhysicallyInstalledSystemMemory(out: *mut u64) -> i32 {
        unsafe { physically_installed_memory(out) }
    }
    Native fn GetLargePageMinimum() -> usize {
        large_page_minimum()
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;

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
    fn system_info_describes_the_host() {
        let mut b = [0u8; 48];
        let mask = processor_mask();
        unsafe {
            GetSystemInfo(std::ptr::null_mut());
            GetSystemInfo(b.as_mut_ptr());
            assert_eq!(u16::from_le_bytes([b[0], b[1]]), 9, "AMD64");
            assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 4096);
            let min = u64::from_le_bytes(b[8..16].try_into().unwrap());
            let max = u64::from_le_bytes(b[16..24].try_into().unwrap());
            assert_eq!(min, 0x1_0000);
            assert!(max > min);
            assert_eq!(u64::from_le_bytes(b[24..32].try_into().unwrap()), mask);
            assert_eq!(
                u32::from_le_bytes(b[32..36].try_into().unwrap()),
                mask.count_ones()
            );
            assert!(u16::from_le_bytes([b[44], b[45]]) >= 5);
            // The native form fills the same structure.
            let mut n = [0u8; 48];
            GetNativeSystemInfo(n.as_mut_ptr());
            assert_eq!(n, b);
        }
    }

    #[test]
    fn system_time_adjustment_is_the_host_clock() {
        let (mut adjust, mut increment, mut disabled) = (0u32, 0u32, -1i32);
        unsafe {
            assert_eq!(
                GetSystemTimeAdjustment(&mut adjust, &mut increment, &mut disabled),
                1
            );
            assert!(increment > 0);
            assert_eq!(adjust, increment);
            assert_eq!(disabled, 1);
            assert_eq!(
                GetSystemTimeAdjustment(std::ptr::null_mut(), &mut increment, &mut disabled),
                0
            );
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_INVALID_PARAMETER
            );
        }
    }

    #[test]
    fn systemtime_is_current_and_well_formed() {
        let mut utc = [0u16; 8];
        let mut local = [0u16; 8];
        unsafe {
            GetSystemTime(utc.as_mut_ptr());
            GetLocalTime(local.as_mut_ptr());
        }
        let year = 1970
            + std::time::SystemTime::now()
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

    #[test]
    fn system_times_are_monotonic() {
        let (mut idle, mut kernel, mut user) = (0u64, 0u64, 0u64);
        assert_eq!(system_times(&mut idle, &mut kernel, &mut user), 1);
        assert!(kernel >= idle);
        assert!(user > 0);
        assert_eq!(
            system_times(std::ptr::null_mut(), &mut kernel, &mut user),
            0
        );
        assert_eq!(unsafe { GetLastError() }, ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn logical_processor_information_ex_needs_a_buffer() {
        let mut need = 0u32;
        unsafe {
            assert_eq!(
                logical_processor_information_ex(
                    RELATION_PROCESSOR_CORE,
                    std::ptr::null_mut(),
                    &mut need
                ),
                0
            );
            assert!(need > 0);
        }
        assert_eq!(unsafe { GetLastError() }, ERROR_INSUFFICIENT_BUFFER);
        let mask = processor_mask();
        let mut buf = vec![0u8; need as usize];
        let mut len = need;
        unsafe {
            assert_eq!(
                logical_processor_information_ex(
                    RELATION_PROCESSOR_CORE,
                    buf.as_mut_ptr(),
                    &mut len
                ),
                1
            );
        }
        assert_eq!(len, need);
        // One record per logical processor, with its size in the header.
        let records = (need as usize) / LPI2_ENTRY;
        assert_eq!(records, mask.count_ones() as usize);
        assert_eq!(
            u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize,
            LPI2_ENTRY
        );
        // The packages query adds one entry.
        let mut len = 0u32;
        unsafe {
            assert_eq!(
                logical_processor_information_ex(RELATION_ALL, std::ptr::null_mut(), &mut len),
                0
            );
        }
        assert_eq!(len as usize, need as usize + LPI2_ENTRY);
        // An unknown relationship is an invalid parameter.
        unsafe {
            assert_eq!(
                logical_processor_information_ex(99, buf.as_mut_ptr(), &mut len),
                0
            );
        }
        assert_eq!(unsafe { GetLastError() }, ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn memory_sizes_are_plausible() {
        let mut b = [0u8; 32];
        b[0] = 32;
        unsafe {
            assert_eq!(GlobalMemoryStatus(b.as_mut_ptr()), 1);
        }
        assert!(u32::from_le_bytes(b[8..12].try_into().unwrap()) > 0);
        unsafe {
            let mut kb = 0u64;
            assert_eq!(GetPhysicallyInstalledSystemMemory(&mut kb), 1);
            assert!(kb > 0);
            assert_eq!(GetPhysicallyInstalledSystemMemory(std::ptr::null_mut()), 0);
        }
        let page = unsafe { GetLargePageMinimum() };
        assert!(page == 0 || page >= 4096);
    }
}
