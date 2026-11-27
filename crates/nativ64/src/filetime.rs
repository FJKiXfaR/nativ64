//! `FILETIME` / `SYSTEMTIME` conversions shared by the file, timezone and time modules.
//!
//! A `FILETIME` is a `u64` of 100 ns intervals since 1601-01-01 UTC; a `SYSTEMTIME` is eight
//! `u16`: year, month, day of week, day, hour, minute, second, millisecond. All conversions here
//! go through the host's C library, so they follow the host's timezone database and calendar
//! (including years before 1970, which the kernel API exposes and Windows' own range starts
//! at 1601).

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
pub(crate) const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

/// 100 ns intervals in one second.
const TICKS: i128 = 10_000_000;

/// The bytes of a `SYSTEMTIME` (`sizeof` is 16 on every Windows architecture).
pub(crate) const SYSTEMTIME_SIZE: usize = 16;

/// Splits a `FILETIME` into Unix seconds (floored) and the remaining 100 ns ticks.
pub(crate) fn split(ft: u64) -> (i64, u32) {
    let rel = i128::from(ft) - i128::from(UNIX_EPOCH_AS_FILETIME);
    (rel.div_euclid(TICKS) as i64, rel.rem_euclid(TICKS) as u32)
}

/// The `FILETIME` of Unix seconds and 100 ns ticks.
pub(crate) fn from_parts(secs: i64, ticks: u32) -> u64 {
    (i128::from(secs) * TICKS + i128::from(ticks) + i128::from(UNIX_EPOCH_AS_FILETIME)) as u64
}

/// The UTC `SYSTEMTIME` of a `FILETIME`, or `None` when the host calendar cannot represent its
/// year (Windows' own range is 1601 through 30827).
pub(crate) fn to_systemtime(ft: u64) -> Option<[u16; SYSTEMTIME_SIZE / 2]> {
    let (secs, ticks) = split(ft);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    // SAFETY: `tm` is a valid out pointer.
    if unsafe { libc::gmtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    let year = tm.tm_year + 1900;
    if !(1601..=30827).contains(&year) {
        return None;
    }
    Some([
        year as u16,
        (tm.tm_mon + 1) as u16,
        tm.tm_wday as u16,
        tm.tm_mday as u16,
        tm.tm_hour as u16,
        tm.tm_min as u16,
        tm.tm_sec as u16,
        (ticks / 10_000) as u16,
    ])
}

/// The Unix seconds of a UTC `SYSTEMTIME`, or `None` for an out-of-range field or a date that
/// does not exist (February 30, a 13th month, hour 24).
pub(crate) fn from_systemtime(st: &[u16]) -> Option<i64> {
    if st.len() < 8 {
        return None;
    }
    // SYSTEMTIME: year, month, day of week, day, hour, minute, second, millisecond.
    let [year, month, _dow, day, hour, minute, second, millis] =
        [st[0], st[1], st[2], st[3], st[4], st[5], st[6], st[7]];
    if !(1601..=30827).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || hour > 23
        || minute > 59
        || second > 59
        || millis > 999
    {
        return None;
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = i32::from(year) - 1900;
    tm.tm_mon = i32::from(month) - 1;
    tm.tm_mday = i32::from(day);
    tm.tm_hour = i32::from(hour);
    tm.tm_min = i32::from(minute);
    tm.tm_sec = i32::from(second);
    // SAFETY: `tm` is fully initialised above; `timegm` normalises the fields.
    let secs = unsafe { libc::timegm(&mut tm) };
    // Reject dates the calendar normalises away (February 30 became March 1 or 2).
    let mut back: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    // SAFETY: `back` is a valid out pointer.
    if unsafe { libc::gmtime_r(&t, &mut back) }.is_null()
        || back.tm_year + 1900 != i32::from(year)
        || back.tm_mon + 1 != i32::from(month)
        || back.tm_mday != i32::from(day)
    {
        return None;
    }
    Some(secs)
}

/// The offset of local time from UTC at Unix time `secs`, in seconds east of UTC.
pub(crate) fn local_offset(secs: i64) -> i64 {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    // SAFETY: `tm` is a valid out pointer.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// A UTC `FILETIME` as a local `FILETIME` (as `FileTimeToLocalFileTime` does).
pub(crate) fn to_local(ft: u64) -> u64 {
    let (secs, ticks) = split(ft);
    from_parts(secs + local_offset(secs), ticks)
}

/// A local `FILETIME` as a UTC one (as `LocalFileTimeToFileTime` does): solves
/// `utc + offset(utc) = local`, trying both sides of a DST change once. A local time that does
/// not exist (the hour skipped when DST starts) maps with the offset in effect before the change.
pub(crate) fn from_local(ft: u64) -> u64 {
    let (local, ticks) = split(ft);
    let mut utc = local - local_offset(local - local_offset(local));
    if utc + local_offset(utc) != local {
        let other = local - local_offset(utc);
        if other + local_offset(other) == local {
            utc = other;
        }
    }
    from_parts(utc, ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip() {
        for (secs, ticks) in [
            (0i64, 0u32),
            (-1, 0),
            (1_234_567_890, 1_234_567),
            (1601i64 - 1970, 0),
            (i64::from(i32::MAX), 9_999_999),
        ] {
            let ft = from_parts(secs, ticks);
            assert_eq!(split(ft), (secs, ticks));
        }
    }

    #[test]
    fn systemtime_known_instants() {
        // 1601-01-01 is FILETIME 0; 1970-01-01 is the epoch constant.
        assert_eq!(
            to_systemtime(0),
            Some([1601, 1, 1, 1, 0, 0, 0, 0]) // 1601-01-01 was a Monday
        );
        assert_eq!(
            to_systemtime(UNIX_EPOCH_AS_FILETIME),
            Some([1970, 1, 4, 1, 0, 0, 0, 0]) // Thursday
        );
        // 2020-02-29 12:34:56.789 UTC (a leap day).
        let ft = from_parts(1_582_979_696, 7_890_000);
        assert_eq!(to_systemtime(ft), Some([2020, 2, 6, 29, 12, 34, 56, 789]));
        assert_eq!(
            from_systemtime(&to_systemtime(ft).unwrap()),
            Some(1_582_979_696)
        );
    }

    #[test]
    fn invalid_systemtime_is_rejected() {
        // February 30 does not exist; month 13, hour 24, day 0 and year 1600 are out of range.
        for st in [
            [2021u16, 2, 0, 30, 0, 0, 0, 0],
            [2021, 13, 0, 1, 0, 0, 0, 0],
            [2021, 1, 0, 1, 24, 0, 0, 0],
            [2021, 1, 0, 0, 0, 0, 0, 0],
            [1600, 1, 0, 1, 0, 0, 0, 0],
        ] {
            assert_eq!(from_systemtime(&st), None, "{st:?}");
        }
    }

    #[test]
    fn local_conversions_are_inverse() {
        for secs in [
            0i64,
            1_000_000_000, // 2001
            1_582_979_696, // 2020-02-29
            1_600_000_000, // 2020-09
            1_700_000_000, // 2023
        ] {
            let ft = from_parts(secs, 0);
            let local = to_local(ft);
            assert_eq!(split(local).0, secs + local_offset(secs));
            assert_eq!(from_local(local), ft, "secs {secs}");
        }
    }

    #[test]
    fn local_offset_matches_libc() {
        for secs in [0i64, 1_582_979_696, 1_700_000_000] {
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            let t = secs as libc::time_t;
            assert!(!unsafe { libc::localtime_r(&t, &mut tm) }.is_null());
            assert_eq!(local_offset(secs), tm.tm_gmtoff as i64);
        }
    }
}
