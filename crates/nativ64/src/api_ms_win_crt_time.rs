//! `api-ms-win-crt-time`: UCRT time functions on top of the Linux clock.
//!
//! ABI: the Windows `struct tm` is nine `int`s (36 bytes, no `tm_gmtoff`/`tm_zone`), so it is
//! converted and never handed to glibc as is. `time_t` is the 64-bit `__time64_t`.
//!
//! Ranges (the UCRT's): `_gmtime64` accepts -43200 ..= 32535291599 (`_MAX__TIME64_T`, 32535244799
//! = 3001-01-01 07:59:59 UTC, plus 13 hours); `_localtime64_s` accepts 0 ..= 32535244799;
//! `_mktime64`/`_mkgmtime64` return -1 outside 0 ..= 32535244799. Out of range, the `_s` variants
//! fill the `tm` with -1, return `EINVAL` and the others return NULL/-1; all of them set `errno`
//! to `EINVAL` and call the invalid parameter handler (`_mktime64`/`_mkgmtime64` set `errno` only).
//!
//! Deviation: local time is the Linux one (glibc `localtime_r`/`mktime`, TZ from the process
//! environment, re-read at each call), not the Windows time zone. `%Z` prints the glibc zone
//! abbreviation (Windows prints the zone's full name) and `%z` the offset (`+hhmm`) at the
//! given time.
//!
//! strftime family: the "C" locale only. The `lc_time` argument of `_Strftime`/`_Wcsftime` is
//! ignored (so the date/time pictures of a custom `__lc_time_data` are not honoured). The UCRT's
//! C locale expands `%c` to `%a %b %e %H:%M:%S %Y`, `%x` to `%m/%d/%y`, `%X` to `%H:%M:%S`
//! (`%#c`: `%A, %B %d, %Y %H:%M:%S`, `%#x`: `%A, %B %d, %Y`, `%#X`: as `%X`). The `tm` fields
//! each specifier reads are range checked (not the others): an invalid field or specifier gives
//! 0, an empty buffer, `EINVAL` and the invalid parameter handler; an
//! output that does not fit gives 0, an empty buffer and `ERANGE`. The `E` and `O` modifiers are
//! accepted and ignored; `#` removes the zero padding of numbers (and selects the long date
//! forms). `%C %D %e %F %g %G %h %n %r %R %t %T %u %V` are supported as in the UCRT.
//!
//! `_Getdays`/`_Getmonths` return a `malloc`ed string `":Sun:Sunday:Mon:Monday:...:Saturday"` /
//! `":Jan:January:...:December"` (every name preceded by a colon); `_W_*` are the UTF-16
//! equivalents. `_Gettnames`/`_W_Gettnames` return a single `malloc`ed block (free with `free`)
//! holding a UCRT `__lc_time_data` ([`LcTimeData`]: narrow name tables, `am`/`pm`, the C locale
//! date/time pictures `MM/dd/yy`, `dddd, MMMM dd, yyyy`, `HH:mm:ss`, `unk` and `refcount` set to
//! 0, then the same in UTF-16 with the locale name "C") whose pointers point inside the block.
//! `_W_Gettnames` returns the same block.

use crate::api_ms_win_crt_runtime::{invalid_parameter_err, set_errno};
use std::cell::Cell;
use std::ffi::{c_char, c_void};

extern "C" {
    /// Re-reads `TZ` (not in the `libc` crate).
    pub(crate) fn tzset();
}

const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

/// 3001-01-01 07:59:59 UTC.
const MAX_TIME64: i64 = 32_535_244_799;
const GMTIME_MIN: i64 = -43_200;
const GMTIME_MAX: i64 = MAX_TIME64 + 46_800;

/// The Windows `struct tm`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tm {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

const TM_INVALID: Tm = Tm {
    tm_sec: -1,
    tm_min: -1,
    tm_hour: -1,
    tm_mday: -1,
    tm_mon: -1,
    tm_year: -1,
    tm_wday: -1,
    tm_yday: -1,
    tm_isdst: -1,
};

/// The Windows `struct __timeb64`.
#[repr(C)]
pub struct Timeb64 {
    pub time: i64,
    pub millitm: u16,
    pub timezone: i16,
    pub dstflag: i16,
    _pad: i16,
}

thread_local! {
    static GMTIME_BUF: Cell<Tm> = const { Cell::new(TM_INVALID) };
}

// ---------------------------------------------------------------------------------------------
// Calendar arithmetic (proleptic Gregorian).

/// Days since 1970-01-01 of a civil date (`m` in 1..=12).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date (year, month 1..=12, day) of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn year_days(y: i64) -> i64 {
    if is_leap(y) {
        366
    } else {
        365
    }
}

/// Broken-down UTC time of `t` (no range check beyond what `i64` arithmetic allows).
fn tm_from_secs(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    Tm {
        tm_sec: (rem % 60) as i32,
        tm_min: (rem / 60 % 60) as i32,
        tm_hour: (rem / 3600) as i32,
        tm_mday: d as i32,
        tm_mon: (m - 1) as i32,
        tm_year: (y - 1900) as i32,
        tm_wday: (days + 4).rem_euclid(7) as i32,
        tm_yday: (days - days_from_civil(y, 1, 1)) as i32,
        tm_isdst: 0,
    }
}

/// UTC seconds of a possibly denormalized `tm` (fields are summed, months carried into years).
fn secs_from_tm(tm: &Tm) -> i64 {
    let mon = i64::from(tm.tm_mon);
    let y = i64::from(tm.tm_year) + 1900 + mon.div_euclid(12);
    let days = days_from_civil(y, mon.rem_euclid(12) + 1, 1) + i64::from(tm.tm_mday) - 1;
    days * 86_400 + i64::from(tm.tm_hour) * 3600 + i64::from(tm.tm_min) * 60 + i64::from(tm.tm_sec)
}

fn to_libc(tm: &Tm) -> libc::tm {
    let mut l: libc::tm = unsafe { std::mem::zeroed() };
    l.tm_sec = tm.tm_sec;
    l.tm_min = tm.tm_min;
    l.tm_hour = tm.tm_hour;
    l.tm_mday = tm.tm_mday;
    l.tm_mon = tm.tm_mon;
    l.tm_year = tm.tm_year;
    l.tm_wday = tm.tm_wday;
    l.tm_yday = tm.tm_yday;
    l.tm_isdst = tm.tm_isdst;
    l
}

fn from_libc(l: &libc::tm) -> Tm {
    Tm {
        tm_sec: l.tm_sec,
        tm_min: l.tm_min,
        tm_hour: l.tm_hour,
        tm_mday: l.tm_mday,
        tm_mon: l.tm_mon,
        tm_year: l.tm_year,
        tm_wday: l.tm_wday,
        tm_yday: l.tm_yday,
        tm_isdst: l.tm_isdst,
    }
}

/// Local broken-down time of `t` (glibc, TZ re-read), with the offset east of UTC in seconds.
fn local_tm(t: i64) -> Option<(Tm, i64)> {
    let tt = libc::time_t::try_from(t).ok()?;
    let mut l: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        tzset();
        if libc::localtime_r(&tt, &mut l).is_null() {
            return None;
        }
    }
    Some((from_libc(&l), l.tm_gmtoff))
}

// ---------------------------------------------------------------------------------------------
// strftime.

const SHORT_DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const SHORT_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

type Res<T> = Result<T, ()>;

fn chk(v: i32, lo: i32, hi: i32) -> Res<i32> {
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(())
    }
}

/// A number padded with zeros to `w` digits, or unpadded with the `#` flag.
fn num(v: i32, w: usize, hash: bool) -> String {
    if hash {
        v.to_string()
    } else {
        format!("{v:0w$}")
    }
}

/// `(year, week)` of the ISO 8601 week of `tm` (from its `tm_yday` and `tm_wday`).
fn iso_week(tm: &Tm) -> (i64, i32) {
    let mut year = i64::from(tm.tm_year) + 1900;
    // Day of the year of this week's Thursday, which decides the week's year.
    let mut thu = i64::from(tm.tm_yday) - i64::from((tm.tm_wday + 6) % 7) + 3;
    if thu < 0 {
        year -= 1;
        thu += year_days(year);
    } else if thu >= year_days(year) {
        thu -= year_days(year);
        year += 1;
    }
    (year, (thu / 7 + 1) as i32)
}

/// UTC offset (seconds east) and zone abbreviation in effect at the local time `tm`.
fn zone_of(tm: &Tm) -> (i64, String) {
    let mut l = to_libc(tm);
    unsafe {
        tzset();
        if libc::mktime(&mut l) == -1 && l.tm_zone.is_null() {
            return (0, "UTC".into());
        }
        let name = if l.tm_zone.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(l.tm_zone)
                .to_string_lossy()
                .into_owned()
        };
        (l.tm_gmtoff, name)
    }
}

/// Expands a format made of specifiers only (used by the composite specifiers).
fn expand(fmt: &str, tm: &Tm) -> Res<String> {
    let mut out = String::new();
    let mut it = fmt.bytes();
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(c as char);
            continue;
        }
        let mut c = it.next().ok_or(())?;
        let hash = c == b'#';
        if hash {
            c = it.next().ok_or(())?;
        }
        out.push_str(&spec(c, hash, tm)?);
    }
    Ok(out)
}

/// The expansion of the specifier `%c` (`%#c` when `hash`).
fn spec(c: u8, hash: bool, tm: &Tm) -> Res<String> {
    let h = hash;
    let year_ok = || chk(tm.tm_year, -1900, 8099).map(|y| i64::from(y) + 1900);
    Ok(match c {
        b'a' => SHORT_DAYS[chk(tm.tm_wday, 0, 6)? as usize].into(),
        b'A' => DAYS[chk(tm.tm_wday, 0, 6)? as usize].into(),
        b'b' | b'h' => SHORT_MONTHS[chk(tm.tm_mon, 0, 11)? as usize].into(),
        b'B' => MONTHS[chk(tm.tm_mon, 0, 11)? as usize].into(),
        b'c' if h => expand("%A, %B %d, %Y %H:%M:%S", tm)?,
        b'c' => expand("%a %b %e %H:%M:%S %Y", tm)?,
        b'C' => num((year_ok()? / 100) as i32, 2, h),
        b'd' => num(chk(tm.tm_mday, 1, 31)?, 2, h),
        b'D' if h => expand("%#m/%#d/%#y", tm)?,
        b'D' => expand("%m/%d/%y", tm)?,
        b'e' => {
            let d = chk(tm.tm_mday, 1, 31)?;
            if h {
                d.to_string()
            } else {
                format!("{d:>2}")
            }
        }
        b'F' if h => expand("%#Y-%#m-%#d", tm)?,
        b'F' => expand("%Y-%m-%d", tm)?,
        b'g' | b'G' | b'V' => {
            year_ok()?;
            chk(tm.tm_wday, 0, 6)?;
            chk(tm.tm_yday, 0, 365)?;
            let (y, w) = iso_week(tm);
            match c {
                b'g' => num((y % 100) as i32, 2, h),
                b'G' => num(y as i32, 4, h),
                _ => num(w, 2, h),
            }
        }
        b'H' => num(chk(tm.tm_hour, 0, 23)?, 2, h),
        b'I' => num((chk(tm.tm_hour, 0, 23)? + 11) % 12 + 1, 2, h),
        b'j' => num(chk(tm.tm_yday, 0, 365)? + 1, 3, h),
        b'm' => num(chk(tm.tm_mon, 0, 11)? + 1, 2, h),
        b'M' => num(chk(tm.tm_min, 0, 59)?, 2, h),
        b'n' => "\n".into(),
        b'p' => if chk(tm.tm_hour, 0, 23)? < 12 {
            "AM"
        } else {
            "PM"
        }
        .into(),
        b'r' if h => expand("%#I:%#M:%#S %p", tm)?,
        b'r' => expand("%I:%M:%S %p", tm)?,
        b'R' if h => expand("%#H:%#M", tm)?,
        b'R' => expand("%H:%M", tm)?,
        b'S' => num(chk(tm.tm_sec, 0, 60)?, 2, h),
        b't' => "\t".into(),
        b'T' if h => expand("%#H:%#M:%#S", tm)?,
        b'T' | b'X' => expand("%H:%M:%S", tm)?,
        b'u' => num(
            match chk(tm.tm_wday, 0, 6)? {
                0 => 7,
                d => d,
            },
            1,
            h,
        ),
        b'U' | b'W' => {
            let wd = chk(tm.tm_wday, 0, 6)?;
            let yd = chk(tm.tm_yday, 0, 365)?;
            let first = if c == b'U' { wd } else { (wd + 6) % 7 };
            num((yd + 7 - first) / 7, 2, h)
        }
        b'w' => num(chk(tm.tm_wday, 0, 6)?, 1, h),
        b'x' if h => expand("%A, %B %d, %Y", tm)?,
        b'x' => expand("%m/%d/%y", tm)?,
        b'y' => num((year_ok()? % 100) as i32, 2, h),
        b'Y' => num(year_ok()? as i32, 4, h),
        b'z' => {
            chk(tm.tm_year, -1900, 8099)?;
            let (off, _) = zone_of(tm);
            let a = off.abs() / 60;
            format!(
                "{}{:02}{:02}",
                if off < 0 { '-' } else { '+' },
                a / 60,
                a % 60
            )
        }
        b'Z' => zone_of(tm).1,
        b'%' => "%".into(),
        _ => return Err(()),
    })
}

/// A character unit of a format: `u8` or UTF-16 `u16`.
trait Unit: Copy + From<u8> + Into<u32> {}
impl Unit for u8 {}
impl Unit for u16 {}

/// Expands `fmt` (without its NUL) for `tm`; `Err` on an invalid specifier or field.
fn format_tm<T: Unit>(fmt: &[T], tm: &Tm) -> Res<Vec<T>> {
    let mut out = Vec::with_capacity(fmt.len() * 2);
    let mut i = 0;
    let at = |i: usize| fmt.get(i).map(|&u| u.into());
    while i < fmt.len() {
        let u = fmt[i];
        i += 1;
        if u.into() != u32::from(b'%') {
            out.push(u);
            continue;
        }
        // Modifiers: `E` and `O` are ignored, `#` is a flag.
        let mut hash = false;
        loop {
            match at(i) {
                Some(0x23) if !hash => hash = true,
                Some(0x45 | 0x4F) => {}
                _ => break,
            }
            i += 1;
        }
        let c = at(i).ok_or(())?;
        i += 1;
        let c = u8::try_from(c).map_err(|_| ())?;
        out.extend(spec(c, hash, tm)?.bytes().map(T::from));
    }
    Ok(out)
}

unsafe fn strftime_impl<T: Unit>(buf: *mut T, max: usize, fmt: *const T, tm: *const Tm) -> usize {
    if buf.is_null() || max == 0 {
        invalid_parameter_err(EINVAL);
        return 0;
    }
    *buf = T::from(0);
    if fmt.is_null() || tm.is_null() {
        invalid_parameter_err(EINVAL);
        return 0;
    }
    let mut n = 0;
    while (*fmt.add(n)).into() != 0 {
        n += 1;
    }
    let Ok(out) = format_tm(std::slice::from_raw_parts(fmt, n), &*tm) else {
        invalid_parameter_err(EINVAL);
        return 0;
    };
    if out.len() >= max {
        set_errno(ERANGE);
        return 0;
    }
    std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
    *buf.add(out.len()) = T::from(0);
    out.len()
}

// ---------------------------------------------------------------------------------------------
// _Getdays and friends.

/// The names as `":short:long:short:long..."`.
fn names_string(short: &[&str], long: &[&str]) -> String {
    short
        .iter()
        .zip(long)
        .map(|(s, l)| format!(":{s}:{l}"))
        .collect()
}

unsafe fn malloc_copy<T: Copy>(src: &[T]) -> *mut T {
    let p = libc::malloc(std::mem::size_of_val(src)) as *mut T;
    if p.is_null() {
        set_errno(12); // ENOMEM
    } else {
        std::ptr::copy_nonoverlapping(src.as_ptr(), p, src.len());
    }
    p
}

unsafe fn malloc_str(s: &str) -> *mut c_char {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    malloc_copy(&b) as *mut c_char
}

unsafe fn malloc_wstr(s: &str) -> *mut u16 {
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    malloc_copy(&w)
}

/// The UCRT `__lc_time_data` (as `_Gettnames` returns it): narrow tables, then wide tables.
#[repr(C)]
pub struct LcTimeData {
    pub short_wday: [*const u8; 7],
    pub wday: [*const u8; 7],
    pub short_mon: [*const u8; 12],
    pub mon: [*const u8; 12],
    pub am: *const u8,
    pub pm: *const u8,
    pub short_date: *const u8,
    pub date: *const u8,
    pub time: *const u8,
    pub unk: i32,
    pub refcount: i32,
    pub short_wday_w: [*const u16; 7],
    pub wday_w: [*const u16; 7],
    pub short_mon_w: [*const u16; 12],
    pub mon_w: [*const u16; 12],
    pub am_w: *const u16,
    pub pm_w: *const u16,
    pub short_date_w: *const u16,
    pub date_w: *const u16,
    pub time_w: *const u16,
    pub locname_w: *const u16,
}

/// A `malloc`ed C-locale `LcTimeData` followed by its strings.
unsafe fn new_time_data() -> *mut c_void {
    let mut strs: Vec<&str> = Vec::new();
    strs.extend(SHORT_DAYS);
    strs.extend(DAYS);
    strs.extend(SHORT_MONTHS);
    strs.extend(MONTHS);
    strs.extend(["AM", "PM", "MM/dd/yy", "dddd, MMMM dd, yyyy", "HH:mm:ss"]);
    let head = std::mem::size_of::<LcTimeData>();
    // Narrow strings, then (2-byte aligned) wide strings, plus the wide locale name.
    let mut narrow = Vec::new();
    let mut noff = Vec::new();
    for s in &strs {
        noff.push(head + narrow.len());
        narrow.extend_from_slice(s.as_bytes());
        narrow.push(0);
    }
    if narrow.len() % 2 == 1 {
        narrow.push(0);
    }
    let wbase = head + narrow.len();
    let mut wide: Vec<u16> = Vec::new();
    let mut woff = Vec::new();
    for s in strs.iter().copied().chain(["C"]) {
        woff.push(wbase + wide.len() * 2);
        wide.extend(s.encode_utf16());
        wide.push(0);
    }
    let total = wbase + wide.len() * 2;
    let p = libc::calloc(1, total) as *mut u8;
    if p.is_null() {
        set_errno(12);
        return p as *mut c_void;
    }
    std::ptr::copy_nonoverlapping(narrow.as_ptr(), p.add(head), narrow.len());
    std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, p.add(wbase), wide.len() * 2);
    let n = |i: usize| p.add(noff[i]) as *const u8;
    let w = |i: usize| p.add(woff[i]) as *const u16;
    let d = &mut *(p as *mut LcTimeData);
    for i in 0..7 {
        d.short_wday[i] = n(i);
        d.wday[i] = n(7 + i);
        d.short_wday_w[i] = w(i);
        d.wday_w[i] = w(7 + i);
    }
    for i in 0..12 {
        d.short_mon[i] = n(14 + i);
        d.mon[i] = n(26 + i);
        d.short_mon_w[i] = w(14 + i);
        d.mon_w[i] = w(26 + i);
    }
    d.am = n(38);
    d.pm = n(39);
    d.short_date = n(40);
    d.date = n(41);
    d.time = n(42);
    d.am_w = w(38);
    d.pm_w = w(39);
    d.short_date_w = w(40);
    d.date_w = w(41);
    d.time_w = w(42);
    d.locname_w = w(43);
    p as *mut c_void
}

// ---------------------------------------------------------------------------------------------

/// Fills `*out` with the UTC time of `*t` for `_gmtime64_s`/`_gmtime64`; `false` when invalid.
unsafe fn gm_checked(t: *const i64) -> Option<Tm> {
    if t.is_null() || !(GMTIME_MIN..=GMTIME_MAX).contains(&*t) {
        invalid_parameter_err(EINVAL);
        return None;
    }
    Some(tm_from_secs(*t))
}

win32! {
    module "api-ms-win-crt-time" => "ucrtbase";
    Native fn _time64(out: *mut i64) -> i64 {
        let t = libc::time(std::ptr::null_mut()) as i64;
        if !out.is_null() {
            *out = t;
        }
        t
    }
    Native fn _difftime64(a: i64, b: i64) -> f64 {
        a.wrapping_sub(b) as f64
    }
    Native fn _ftime64(tb: *mut Timeb64) {
        if tb.is_null() {
            invalid_parameter_err(EINVAL);
            return;
        }
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        let (tz, dst) = match local_tm(ts.tv_sec) {
            Some((l, off)) => ((-off / 60) as i16, i16::from(l.tm_isdst > 0)),
            None => (0, 0),
        };
        *tb = Timeb64 {
            time: ts.tv_sec,
            millitm: (ts.tv_nsec / 1_000_000) as u16,
            timezone: tz,
            dstflag: dst,
            _pad: 0,
        };
    }
    Native fn _gmtime64(t: *const i64) -> *mut Tm {
        match gm_checked(t) {
            Some(tm) => GMTIME_BUF.with(|b| {
                b.set(tm);
                b.as_ptr()
            }),
            None => std::ptr::null_mut(),
        }
    }
    Native fn _gmtime64_s(out: *mut Tm, t: *const i64) -> i32 {
        if out.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        *out = TM_INVALID;
        match gm_checked(t) {
            Some(tm) => {
                *out = tm;
                0
            }
            None => EINVAL,
        }
    }
    Native fn _localtime64_s(out: *mut Tm, t: *const i64) -> i32 {
        if out.is_null() {
            return invalid_parameter_err(EINVAL);
        }
        *out = TM_INVALID;
        if t.is_null() || !(0..=MAX_TIME64).contains(&*t) {
            return invalid_parameter_err(EINVAL);
        }
        match local_tm(*t) {
            Some((tm, _)) => {
                *out = tm;
                0
            }
            None => invalid_parameter_err(EINVAL),
        }
    }
    Native fn _mktime64(tm: *mut Tm) -> i64 {
        if tm.is_null() {
            invalid_parameter_err(EINVAL);
            return -1;
        }
        let mut l = to_libc(&*tm);
        tzset();
        let t = libc::mktime(&mut l) as i64;
        if !(0..=MAX_TIME64).contains(&t) {
            set_errno(EINVAL);
            return -1;
        }
        *tm = from_libc(&l);
        t
    }
    Native fn _mkgmtime64(tm: *mut Tm) -> i64 {
        if tm.is_null() {
            invalid_parameter_err(EINVAL);
            return -1;
        }
        let t = secs_from_tm(&*tm);
        if !(0..=MAX_TIME64).contains(&t) {
            set_errno(EINVAL);
            return -1;
        }
        *tm = tm_from_secs(t);
        t
    }
    Native fn strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize {
        strftime_impl(buf, max, fmt, tm)
    }
    Native fn _Strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const Tm, _lc_time: *const c_void) -> usize {
        strftime_impl(buf, max, fmt, tm)
    }
    Native fn _Wcsftime(buf: *mut u16, max: usize, fmt: *const u16, tm: *const Tm, _lc_time: *const c_void) -> usize {
        strftime_impl(buf, max, fmt, tm)
    }
    Native fn _Getdays() -> *mut c_char {
        malloc_str(&names_string(&SHORT_DAYS, &DAYS))
    }
    Native fn _Getmonths() -> *mut c_char {
        malloc_str(&names_string(&SHORT_MONTHS, &MONTHS))
    }
    Native fn _Gettnames() -> *mut c_void {
        new_time_data()
    }
    Native fn _W_Getdays() -> *mut u16 {
        malloc_wstr(&names_string(&SHORT_DAYS, &DAYS))
    }
    Native fn _W_Getmonths() -> *mut u16 {
        malloc_wstr(&names_string(&SHORT_MONTHS, &MONTHS))
    }
    Native fn _W_Gettnames() -> *mut c_void {
        new_time_data()
    }
}

#[cfg(test)]
pub(crate) mod test_tz {
    //! Serializes tests that change `TZ` and restores it.
    use super::tzset;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    pub struct TzGuard(
        Option<std::ffi::OsString>,
        #[allow(dead_code)] MutexGuard<'static, ()>,
    );

    pub fn set_tz(tz: &str) -> TzGuard {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("TZ");
        std::env::set_var("TZ", tz);
        unsafe { tzset() };
        TzGuard(old, g)
    }

    impl Drop for TzGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("TZ", v),
                None => std::env::remove_var("TZ"),
            }
            unsafe { tzset() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_tz::set_tz;
    use super::*;
    use crate::api_ms_win_crt_runtime::{_set_errno, errno, test_support::*};

    const NAMES: &str = "_time64 _difftime64 _ftime64 _gmtime64 _gmtime64_s _localtime64_s _mktime64 _mkgmtime64 strftime _Strftime _Wcsftime _Getdays _Getmonths _Gettnames _W_Getdays _W_Getmonths _W_Gettnames";

    #[allow(clippy::too_many_arguments)]
    fn tm(
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
    ) -> Tm {
        Tm {
            tm_sec: sec,
            tm_min: min,
            tm_hour: hour,
            tm_mday: mday,
            tm_mon: mon,
            tm_year: year,
            tm_wday: wday,
            tm_yday: yday,
            tm_isdst: 0,
        }
    }

    fn fmt(f: &str, t: &Tm) -> String {
        let f = std::ffi::CString::new(f).unwrap();
        let mut buf = [1u8; 256];
        let n = unsafe { strftime(buf.as_mut_ptr(), buf.len(), f.as_ptr() as *const u8, t) };
        assert_eq!(buf[n], 0);
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn registered_both_names() {
        assert_eq!(NAMES.split(' ').count(), 17);
        for n in NAMES.split(' ') {
            assert!(
                crate::lookup("api-ms-win-crt-time-l1-1-0.dll", n).is_some(),
                "{n}"
            );
            assert!(crate::lookup("ucrtbase", n).is_some(), "{n}");
        }
    }

    #[test]
    fn layouts() {
        assert_eq!(std::mem::size_of::<Tm>(), 36);
        assert_eq!(std::mem::size_of::<Timeb64>(), 16);
        assert_eq!(std::mem::offset_of!(Timeb64, dstflag), 12);
        assert_eq!(std::mem::size_of::<LcTimeData>(), 704);
    }

    #[test]
    fn gmtime_values_and_range() {
        unsafe {
            let t = 0i64;
            let p = _gmtime64(&t);
            assert_eq!(*p, tm(0, 0, 0, 1, 0, 70, 4, 0));
            for (t, want) in [
                (-1i64, tm(59, 59, 23, 31, 11, 69, 3, 364)),
                (-43200, tm(0, 0, 12, 31, 11, 69, 3, 364)),
                (MAX_TIME64 + 46_800, tm(59, 59, 20, 1, 0, 1101, 0, 0)),
                (951_782_400, tm(0, 0, 0, 29, 1, 100, 2, 59)),
            ] {
                let mut o = TM_INVALID;
                assert_eq!(_gmtime64_s(&mut o, &t), 0);
                if t == MAX_TIME64 + 46_800 {
                    assert_eq!(
                        (o.tm_year, o.tm_hour, o.tm_min, o.tm_sec),
                        (1101, 20, 59, 59)
                    );
                } else {
                    assert_eq!(o, want, "{t}");
                }
            }
            let a = 1i64;
            let b = 2i64;
            let pa = _gmtime64(&a);
            let pb = _gmtime64(&b);
            assert_eq!(pa, pb, "per-thread buffer is reused");
            assert_eq!((*pb).tm_sec, 2);
        }
    }

    #[test]
    fn gmtime_errors() {
        let _g = install_counting_handler();
        unsafe {
            for t in [-43201i64, GMTIME_MAX + 1] {
                _set_errno(0);
                assert!(_gmtime64(&t).is_null());
                assert_eq!(errno(), EINVAL);
                let mut o = tm(0, 0, 0, 0, 0, 0, 0, 0);
                assert_eq!(_gmtime64_s(&mut o, &t), EINVAL);
                assert_eq!(o, TM_INVALID);
            }
            assert!(_gmtime64(std::ptr::null()).is_null());
            let mut o = tm(0, 0, 0, 0, 0, 0, 0, 0);
            assert_eq!(_gmtime64_s(&mut o, std::ptr::null()), EINVAL);
            assert_eq!(o, TM_INVALID);
            assert_eq!(_gmtime64_s(std::ptr::null_mut(), &0), EINVAL);
        }
        assert_eq!(calls(), 7);
    }

    #[test]
    fn mkgmtime_roundtrip_and_normalization() {
        unsafe {
            let mut t = tm(0, 0, 0, 1, 0, 70, 0, 0);
            t.tm_isdst = 1;
            assert_eq!(_mkgmtime64(&mut t), 0);
            assert_eq!((t.tm_wday, t.tm_yday, t.tm_isdst), (4, 0, 0));
            let mut t = tm(61, 59, 23, 31, 12, 99, 0, 0); // 1999-13-31 23:59:61
            assert_eq!(_mkgmtime64(&mut t), 949_363_201); // 2000-01-31 00:00:01
            assert_eq!(t, tm(1, 0, 0, 1, 1, 100, 2, 31));
            let mut t = tm(1, 0, 0, 2, 0, 70, 0, 0);
            assert_eq!(_mkgmtime64(&mut t), 86_401);
            assert_eq!((t.tm_wday, t.tm_yday), (5, 1));
            let mut t = tm(0, 0, 0, 1, 0, 69, 0, 0);
            _set_errno(0);
            assert_eq!(_mkgmtime64(&mut t), -1);
            assert_eq!(errno(), EINVAL);
            let mut t = tm(0, 0, 0, 1, 0, 1102, 0, 0);
            assert_eq!(_mkgmtime64(&mut t), -1);
        }
        let _g = install_counting_handler();
        unsafe {
            assert_eq!(_mkgmtime64(std::ptr::null_mut()), -1);
            assert_eq!(_mktime64(std::ptr::null_mut()), -1);
        }
        assert_eq!(calls(), 2);
    }

    #[test]
    fn local_time_follows_tz() {
        let _tz = set_tz("EST5EDT");
        unsafe {
            let summer = 1_720_000_000i64; // 2024-07-03 09:46:40 UTC
            let mut o = TM_INVALID;
            assert_eq!(_localtime64_s(&mut o, &summer), 0);
            assert_eq!((o.tm_hour, o.tm_min, o.tm_isdst), (5, 46, 1));
            let winter = 1_705_000_000i64;
            assert_eq!(_localtime64_s(&mut o, &winter), 0);
            assert_eq!((o.tm_hour, o.tm_isdst), (14, 0));
            // mktime inverts it, normalizes, and honours the isdst field.
            let mut t = tm(0, 120, 3, 3, 6, 124, 0, 0); // 03:120 -> 05:00
            t.tm_isdst = -1;
            assert_eq!(_mktime64(&mut t), 1_720_000_000 - 46 * 60 - 40);
            assert_eq!((t.tm_hour, t.tm_min, t.tm_wday, t.tm_isdst), (5, 0, 3, 1));
            let mut z = tm(0, 0, 12, 1, 0, 70, 0, 0);
            assert_eq!(_mktime64(&mut z), 5 * 3600 + 12 * 3600);
            // Before the epoch in local time: -1.
            let mut z = tm(0, 0, 0, 1, 0, 69, 0, 0);
            assert_eq!(_mktime64(&mut z), -1);
        }
    }

    #[test]
    fn localtime_errors() {
        let _tz = set_tz("UTC");
        let _g = install_counting_handler();
        unsafe {
            let mut o = tm(0, 0, 0, 0, 0, 0, 0, 0);
            assert_eq!(
                _localtime64_s(std::ptr::null_mut(), std::ptr::null()),
                EINVAL
            );
            assert_eq!(_localtime64_s(&mut o, std::ptr::null()), EINVAL);
            assert_eq!(o, TM_INVALID);
            for t in [-1i64, MAX_TIME64 + 1] {
                o = tm(0, 0, 0, 0, 0, 0, 0, 0);
                _set_errno(0);
                assert_eq!(_localtime64_s(&mut o, &t), EINVAL);
                assert_eq!(errno(), EINVAL);
                assert_eq!(o, TM_INVALID);
            }
            assert_eq!(_localtime64_s(&mut o, &MAX_TIME64), 0);
            assert_eq!(o.tm_year, 1101);
        }
        assert_eq!(calls(), 4);
    }

    #[test]
    fn time_difftime_ftime() {
        unsafe {
            let mut t = 0i64;
            let r = _time64(&mut t);
            assert_eq!(r, t);
            assert!(
                (r - std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64)
                    .abs()
                    <= 2
            );
            assert!(_time64(std::ptr::null_mut()) > 1_700_000_000);
            assert_eq!(_difftime64(10, 4), 6.0);
            assert_eq!(_difftime64(4, 10), -6.0);
            let _tz = set_tz("EST5EDT");
            let mut tb = Timeb64 {
                time: 0,
                millitm: 9999,
                timezone: 9999,
                dstflag: 9999,
                _pad: 0,
            };
            _ftime64(&mut tb);
            assert!(tb.time > 1_700_000_000 && tb.millitm < 1000);
            assert!(
                matches!((tb.timezone, tb.dstflag), (300, 0) | (240, 1)),
                "{} {}",
                tb.timezone,
                tb.dstflag
            );
        }
        let _g = install_counting_handler();
        unsafe { _ftime64(std::ptr::null_mut()) };
        assert_eq!(calls(), 1);
    }

    /// (format, expected or "" for an error, tm).
    #[test]
    fn strftime_table() {
        let t70 = tm(0, 0, 0, 1, 0, 70, 4, 0);
        let cases: Vec<(&str, &str, Tm)> = vec![
            ("%C", "", tm(0, 0, 0, 1, 0, -2000, 4, 0)),
            ("%C", "00", tm(0, 0, 0, 1, 0, -1900, 4, 0)),
            ("%C", "18", tm(0, 0, 0, 1, 0, -1, 4, 0)),
            ("%C", "99", tm(0, 0, 0, 1, 0, 8099, 4, 0)),
            ("%C", "", tm(0, 0, 0, 1, 0, 8100, 4, 0)),
            ("%d", "", tm(0, 0, 0, 0, 0, 70, 4, 0)),
            ("%d", "31", tm(0, 0, 0, 31, 0, 70, 4, 0)),
            ("%d", "", tm(0, 0, 0, 32, 0, 70, 4, 0)),
            ("%D", "01/01/99", tm(0, 0, 0, 1, 0, -1, 4, 0)),
            ("%D", "", tm(0, 0, 0, 1, 0, 8100, 4, 0)),
            ("%#D", "1/1/70", t70),
            ("%e", " 1", t70),
            ("%#e", "1", t70),
            ("%F", "1970-01-01", t70),
            ("%#F", "1970-1-1", t70),
            ("%R", "00:00", t70),
            ("%#R", "0:0", t70),
            ("%T", "00:00:00", t70),
            ("%#T", "0:0:0", t70),
            ("%u", "", tm(0, 0, 0, 1, 0, 117, -1, 0)),
            ("%u", "7", tm(0, 0, 0, 1, 0, 117, 0, 0)),
            ("%u", "6", tm(0, 0, 0, 1, 0, 117, 6, 0)),
            ("%u", "", tm(0, 0, 0, 1, 0, 117, 7, 0)),
            ("%h", "Jan", t70),
            ("%I", "", tm(0, 0, -1, 1, 0, 70, 4, 0)),
            ("%I", "12", t70),
            ("%I", "01", tm(0, 0, 13, 1, 0, 70, 4, 0)),
            ("%I", "11", tm(0, 0, 23, 1, 0, 70, 4, 0)),
            ("%I", "", tm(0, 0, 24, 1, 0, 70, 4, 0)),
            ("%n", "\n", t70),
            ("%t", "\t", t70),
            ("%r", "12:00:00 AM", t70),
            ("%r", "02:00:00 PM", tm(0, 0, 14, 1, 0, 121, 6, 0)),
            ("%#r", "12:0:0 AM", t70),
            ("%#r", "2:0:0 PM", tm(0, 0, 14, 1, 0, 121, 6, 0)),
            ("%g", "", tm(0, 0, 0, 1, 0, -1901, 4, 0)),
            ("%g", "00", tm(0, 0, 0, 1, 0, -1900, 4, 0)),
            ("%g", "70", t70),
            ("%g", "71", tm(0, 0, 0, 2, 0, 72, 0, 1)),
            ("%g", "16", tm(0, 0, 0, 1, 0, 117, 0, 0)),
            ("%g", "99", tm(0, 0, 0, 1, 0, 8099, 4, 0)),
            ("%g", "00", tm(0, 0, 0, 1, 0, 8099, 3, 364)),
            ("%g", "", tm(0, 0, 0, 1, 0, 8100, 0, 0)),
            ("%G", "1970", t70),
            ("%G", "1972", tm(0, 0, 0, 3, 0, 72, 1, 2)),
            ("%V", "01", t70),
            ("%V", "52", tm(0, 0, 0, 1, 0, 117, 0, 0)),
            ("%V", "53", tm(0, 0, 14, 1, 0, 121, 6, 0)),
            ("%y", "", tm(0, 0, 0, 0, 0, -1901, 0, 0)),
            ("%y", "00", tm(0, 0, 0, 0, 0, -1900, 0, 0)),
            ("%y", "99", tm(0, 0, 0, 0, 0, 8099, 0, 0)),
            ("%y", "70", tm(0, 0, 0, 1, 0, 170, 3, 0)),
            ("%c", "Thu Jan  1 00:00:00 1970", t70),
            (
                "%c",
                "Thu Feb 30 00:00:00 1970",
                tm(0, 0, 0, 30, 1, 70, 4, 0),
            ),
            ("%#c", "Thursday, January 01, 1970 00:00:00", t70),
            ("%x", "01/01/70", t70),
            ("%x", "02/30/70", tm(0, 0, 0, 30, 1, 70, 4, 0)),
            (
                "%#x",
                "Thursday, February 30, 1970",
                tm(0, 0, 0, 30, 1, 70, 4, 0),
            ),
            ("%#x", "", tm(0, 0, 0, 30, 1, 70, 7, 0)),
            ("%#x", "", tm(0, 0, 0, 30, 12, 70, 4, 0)),
            ("%X", "14:00:00", tm(0, 0, 14, 1, 0, 70, 4, 0)),
            ("%X", "23:59:60", tm(60, 59, 23, 1, 0, 70, 4, 0)),
            ("%Oe", "12", tm(0, 0, 0, 12, 0, 70, 0, 0)),
            ("%OH", "23", tm(0, 0, 23, 0, 0, 70, 0, 0)),
            ("%Ey", "70", t70),
            ("%U", "05", tm(0, 0, 0, 1, 1, 70, 0, 31)),
            ("%U", "00", t70),
            ("%W", "04", tm(0, 0, 0, 1, 0, 70, 0, 31)),
            ("%U", "53", tm(0, 0, 0, 1, 0, 70, 0, 365)),
            ("%W", "52", tm(0, 0, 0, 1, 0, 70, 0, 365)),
            ("%w", "4", t70),
            ("%j", "001", t70),
            ("%#j", "1", t70),
            ("%a %A %b %B", "Thu Thursday Jan January", t70),
            ("%p", "AM", t70),
            ("%#M|%#d|%#H", "0|1|0", t70),
            ("e%#%e", "e%e", t70),
            ("100%%", "100%", t70),
            ("%Y", "1970", t70),
            ("%#Y", "1970", t70),
            ("%q", "", t70),
            ("%", "", t70),
            ("%#", "", t70),
            ("a%e", "a 1", t70), // valid in the UCRT
            ("a%0", "", t70),
        ];
        let _g = install_counting_handler();
        let mut errs = 0;
        for (f, want, t) in &cases {
            let before = calls();
            let got = fmt(f, t);
            if want.is_empty() {
                assert_eq!(got, "", "{f}");
                assert_eq!(calls(), before + 1, "{f} must call the handler");
                errs += 1;
            } else {
                assert_eq!(&got, want, "{f} {t:?}");
                assert_eq!(calls(), before, "{f}");
            }
        }
        assert!(errs > 15);
    }

    #[test]
    fn iso_weeks_from_year_and_yday() {
        // (year, yday, "%g %V" for wday 0..=6)
        let rows: [(i32, i32, [&str; 7]); 6] = [
            (
                100,
                0,
                [
                    "99 52", "00 01", "00 01", "00 01", "00 01", "99 53", "99 52",
                ],
            ),
            (
                100,
                4,
                [
                    "00 01", "00 02", "00 01", "00 01", "00 01", "00 01", "00 01",
                ],
            ),
            (
                100,
                361,
                [
                    "00 52", "00 53", "00 52", "00 52", "00 52", "00 52", "00 52",
                ],
            ),
            (
                100,
                363,
                [
                    "00 52", "01 01", "00 53", "00 53", "00 52", "00 52", "00 52",
                ],
            ),
            (
                101,
                362,
                [
                    "01 52", "02 01", "01 53", "01 52", "01 52", "01 52", "01 52",
                ],
            ),
            (
                101,
                364,
                [
                    "01 52", "02 01", "02 01", "02 01", "01 53", "01 52", "01 52",
                ],
            ),
        ];
        for (year, yday, want) in rows {
            for (wday, w) in want.iter().enumerate() {
                let t = tm(0, 0, 0, 1, 0, year, wday as i32, yday);
                assert_eq!(&fmt("%g %V", &t), w, "{year} {yday} {wday}");
            }
        }
    }

    #[test]
    fn strftime_zone_and_wide() {
        let _tz = set_tz("EST5EDT");
        let t = tm(0, 0, 12, 3, 6, 124, 3, 184);
        let mut t = t;
        t.tm_isdst = 1;
        assert_eq!(fmt("%z", &t), "-0400");
        assert_eq!(fmt("%Z", &t), "EDT");
        t.tm_isdst = 0;
        t.tm_mon = 0;
        t.tm_mday = 3;
        t.tm_yday = 2;
        assert_eq!(fmt("%z %Z", &t), "-0500 EST");
        let f: Vec<u16> = "%Y-%m-%d \u{e9}%%".encode_utf16().chain([0]).collect();
        let mut buf = [1u16; 32];
        unsafe {
            let n = _Wcsftime(buf.as_mut_ptr(), 32, f.as_ptr(), &t, std::ptr::null());
            let s = String::from_utf16(&buf[..n]).unwrap();
            assert_eq!(s, "2024-01-03 \u{e9}%");
            assert_eq!(buf[n], 0);
            let c = c"%a";
            let n = _Strftime(
                buf.as_mut_ptr() as *mut u8,
                8,
                c.as_ptr() as *const u8,
                &t,
                std::ptr::null(),
            );
            assert_eq!(n, 3);
        }
    }

    #[test]
    fn strftime_buffer_and_argument_errors() {
        let _g = install_counting_handler();
        let t = tm(0, 0, 0, 1, 0, 70, 4, 0);
        unsafe {
            let mut buf = [b'x'; 8];
            let f = c"copy".as_ptr() as *const u8;
            assert_eq!(strftime(buf.as_mut_ptr(), 5, f, &t), 4);
            assert_eq!(&buf[..5], b"copy\0");
            _set_errno(0);
            assert_eq!(strftime(buf.as_mut_ptr(), 4, f, &t), 0);
            assert_eq!(errno(), ERANGE);
            assert_eq!(buf[0], 0);
            assert_eq!(calls(), 0);
            assert_eq!(strftime(buf.as_mut_ptr(), 2, f, &t), 0);
            _set_errno(0);
            assert_eq!(strftime(std::ptr::null_mut(), 0, f, &t), 0);
            assert_eq!(errno(), EINVAL);
            assert_eq!(strftime(buf.as_mut_ptr(), 0, f, &t), 0);
            assert_eq!(strftime(buf.as_mut_ptr(), 8, f, std::ptr::null()), 0);
            assert_eq!(strftime(buf.as_mut_ptr(), 8, std::ptr::null(), &t), 0);
            assert_eq!(calls(), 4);
        }
    }

    #[test]
    fn day_and_month_names() {
        unsafe {
            let d = _Getdays();
            let s = std::ffi::CStr::from_ptr(d).to_str().unwrap();
            assert_eq!(s, ":Sun:Sunday:Mon:Monday:Tue:Tuesday:Wed:Wednesday:Thu:Thursday:Fri:Friday:Sat:Saturday");
            libc::free(d as *mut c_void);
            let m = _Getmonths();
            let s = std::ffi::CStr::from_ptr(m).to_str().unwrap();
            assert!(s.starts_with(":Jan:January:Feb:February:") && s.ends_with(":Dec:December"));
            assert_eq!(s.matches(':').count(), 24);
            libc::free(m as *mut c_void);
            let w = _W_Getdays();
            let mut n = 0;
            while *w.add(n) != 0 {
                n += 1;
            }
            assert_eq!(String::from_utf16(std::slice::from_raw_parts(w, n)).unwrap(), ":Sun:Sunday:Mon:Monday:Tue:Tuesday:Wed:Wednesday:Thu:Thursday:Fri:Friday:Sat:Saturday");
            libc::free(w as *mut c_void);
            let w = _W_Getmonths();
            assert_eq!(*w, b':' as u16);
            libc::free(w as *mut c_void);
        }
    }

    #[test]
    fn time_names_block() {
        unsafe {
            for p in [_Gettnames(), _W_Gettnames()] {
                let d = &*(p as *const LcTimeData);
                let cs = |p: *const u8| {
                    std::ffi::CStr::from_ptr(p as *const c_char)
                        .to_str()
                        .unwrap()
                        .to_string()
                };
                let ws = |p: *const u16| {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    String::from_utf16(std::slice::from_raw_parts(p, n)).unwrap()
                };
                assert_eq!(cs(d.short_wday[0]), "Sun");
                assert_eq!(cs(d.wday[6]), "Saturday");
                assert_eq!(cs(d.short_mon[11]), "Dec");
                assert_eq!(cs(d.mon[0]), "January");
                assert_eq!((cs(d.am), cs(d.pm)), ("AM".into(), "PM".into()));
                assert_eq!(cs(d.short_date), "MM/dd/yy");
                assert_eq!(cs(d.date), "dddd, MMMM dd, yyyy");
                assert_eq!(cs(d.time), "HH:mm:ss");
                assert_eq!((d.unk, d.refcount), (0, 0));
                assert_eq!(ws(d.wday_w[3]), "Wednesday");
                assert_eq!(ws(d.mon_w[11]), "December");
                assert_eq!(ws(d.pm_w), "PM");
                assert_eq!(ws(d.time_w), "HH:mm:ss");
                assert_eq!(ws(d.locname_w), "C");
                assert_eq!(d.short_mon_w[0] as usize % 2, 0);
                libc::free(p);
            }
        }
    }
}
