//! Frame pacing: `SetFrameIntervalX`, `ScheduleFrameEventX`, `WaitFrameEventX`,
//! `GetFrameStatisticsX` (device slots 78 to 81).
//!
//! Model: a software clock started by the first interval set, ticking once per interval. Each
//! event kind (origin, vblank, flip, ...) happens once per tick: a wait takes the first tick
//! after the last one taken for its kind (or the latest tick already passed, so a late wait
//! ends at once), sleeps until it, and returns. An origin wait then hands out the next frame
//! token (frames are numbered 1, 2, ...); with `TOKEN_ON_TIMEOUT` a timed-out origin wait
//! makes a token too.
//!
//! Shortcuts: event offsets are ignored (every event is at the tick); no statistics are kept.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::xobj::{HRESULT, S_OK};

/// What a wait returns when its time is up first.
pub const TIMEOUT: HRESULT = 1;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out.
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);
/// Per event kind (origin, vblank, flip and later kinds): the last tick a wait for it took.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn clock() -> MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// Frames begun so far.
pub fn current_frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Slot 78: `SetFrameIntervalX(output, length_us, period, flags)`. The period (how many
/// intervals a frame may take to reach the display) is not part of the rate.
///
/// # Safety
/// Plain call.
pub unsafe extern "win64" fn set_frame_interval(
    _this: *mut c_void,
    _output: *mut c_void,
    length_us: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length_us {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Slot 79: `ScheduleFrameEventX(kind, offset, list, flags)`: events happen at every tick.
///
/// # Safety
/// Plain call.
pub unsafe extern "win64" fn schedule_frame_event(
    _this: *mut c_void,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// The tick a wait for `kind` ends on and how long until it. A wait that comes within `limit`
/// takes the tick here, under the clock's lock, so two threads never take the same one.
fn next_tick(kind: u32, limit: Duration) -> (u64, Duration) {
    let mut guard = clock();
    let c = guard.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick = (c.first + (elapsed / interval) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
    if wait <= limit {
        last(kind).fetch_max(tick, Ordering::Relaxed);
    }
    (tick, wait)
}

/// Slot 80: `WaitFrameEventX(kind, timeout_ms, list, flags, *token)`.
///
/// # Safety
/// `token` is NULL or writable.
pub unsafe extern "win64" fn wait_frame_event(
    _this: *mut c_void,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let limit = Duration::from_millis(u64::from(timeout_ms));
    let (_, wait) = next_tick(kind, limit);
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        S_OK
    };
    if kind == ORIGIN && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            *token = frame;
        }
    }
    hr
}

/// Slot 81: `GetFrameStatisticsX(token, kinds, *count, out)`: no statistics, `*count = 0`.
///
/// # Safety
/// `count` is NULL or writable.
pub unsafe extern "win64" fn get_frame_statistics(
    _this: *mut c_void,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        *count = 0;
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: *mut c_void = std::ptr::null_mut();

    /// The clock is global, so one test drives it.
    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        unsafe {
            assert_eq!(set_frame_interval(N, N, 20_000, 1, 0), S_OK);
            let (mut first, mut second) = (0, 0);
            let w = |kind, ms, flags, t: &mut u64| {
                wait_frame_event(N, kind, ms, std::ptr::null(), flags, t)
            };
            assert_eq!(w(0, 1_000, 0, &mut first), S_OK);
            assert_eq!(w(0, 1_000, 0, &mut second), S_OK);
            assert!(second > first);
            // Nothing is ready within 0 ms: time-out, no token.
            let mut none = 77;
            assert_eq!(w(0, 0, 0, &mut none), TIMEOUT);
            assert_eq!(none, 77);
            // A frame started early on a time-out still gets a token.
            let mut early = 0;
            assert_eq!(w(0, 0, TOKEN_ON_TIMEOUT, &mut early), TIMEOUT);
            assert!(early > second);
            // Flip waits each take their own tick.
            let mut t = 0;
            assert_eq!(w(2, 1_000, 0, &mut t), S_OK);
            let a = LAST[2].load(Ordering::Relaxed);
            assert_eq!(w(2, 1_000, 0, &mut t), S_OK);
            assert_eq!(LAST[2].load(Ordering::Relaxed), a + 1);
            // Null token is tolerated.
            assert_eq!(
                wait_frame_event(N, 0, 1_000, std::ptr::null(), 0, std::ptr::null_mut()),
                S_OK
            );
            assert!(current_frame() >= early);
        }
    }

    #[test]
    fn schedule_and_statistics() {
        unsafe {
            assert_eq!(schedule_frame_event(N, 0, 0, std::ptr::null(), 0), S_OK);
            let mut count = 5;
            assert_eq!(
                get_frame_statistics(N, 1, 1, &mut count, std::ptr::null_mut()),
                S_OK
            );
            assert_eq!(count, 0);
            assert_eq!(
                get_frame_statistics(N, 1, 1, std::ptr::null_mut(), std::ptr::null_mut()),
                S_OK
            );
        }
    }

    #[test]
    fn waits_begun_together_take_different_ticks() {
        let long = Duration::from_secs(1);
        let (a, _) = next_tick(1, long);
        let (b, _) = next_tick(1, long);
        assert!(b > a);
        next_tick(1, Duration::ZERO);
        assert_eq!(last(1).load(Ordering::Relaxed), b);
    }
}
