//! `PresentX` (queue slot 32): the end of a frame.
//!
//! Model: all earlier work is complete (null backend), so a flip is immediate. A present never
//! fails for the title: it counts the frame, keeps the last flips in a ring, and logs what it
//! saw (raw plane words, the target resource) so the plane layout can be checked against the
//! title. Later the backend copies plane 0 to the window (docs/gpu.md, G3).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use crate::d3d12::resource::desc_of;
use crate::xobj::{HRESULT, S_OK};

/// Size of one plane's parameters, in bytes (unverified beyond the first plane).
const PLANE_SIZE: usize = 0x48;
/// How many flips the ring keeps.
const RING: usize = 16;
/// How many bytes of `params` are logged.
const PARAMS_LOG: usize = 0x40;

#[derive(Default)]
struct State {
    frames: u64,
    ring: VecDeque<(u64, Instant)>,
    /// Plane count of the last logged call (`None` before the first).
    planes: Option<u32>,
    logged_empty: bool,
    logged_foreign: bool,
    last_lists: u64,
    last_commands: u64,
}

static STATE: Mutex<State> = Mutex::new(State {
    frames: 0,
    ring: VecDeque::new(),
    planes: None,
    logged_empty: false,
    logged_foreign: false,
    last_lists: 0,
    last_commands: 0,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Frames presented so far.
pub fn frames() -> u64 {
    state().frames
}

/// The tokens of the last flips, oldest first.
pub fn recent_tokens() -> Vec<u64> {
    state().ring.iter().map(|&(t, _)| t).collect()
}

/// Queue slot 32 once the queue's counters are read: `lists` and `commands` are its totals.
///
/// # Safety
/// `planes` is NULL or `plane_count` planes of 0x48 bytes; each plane's `resources` is NULL or
/// holds `resource_count` pointers; `params` is NULL or readable for 0x40 bytes.
pub(crate) unsafe fn present_x(
    plane_count: u32,
    planes: *const u8,
    params: *const u8,
    lists: u64,
    commands: u64,
) -> HRESULT {
    let mut st = state();
    let st = &mut *st;
    if plane_count == 0 || planes.is_null() {
        if !st.logged_empty {
            st.logged_empty = true;
            eprintln!("nativ64: PresentX with {plane_count} planes at {planes:p}: nothing to show");
        }
        return S_OK;
    }
    if st.planes != Some(plane_count) {
        st.planes = Some(plane_count);
        for i in 0..plane_count as usize {
            let words: Vec<String> = (0..PLANE_SIZE / 8)
                .map(|w| {
                    let v = planes
                        .add(i * PLANE_SIZE + w * 8)
                        .cast::<u64>()
                        .read_unaligned();
                    format!("{v:#x}")
                })
                .collect();
            eprintln!(
                "nativ64: PresentX plane {i}/{plane_count}: [{}]",
                words.join(" ")
            );
        }
        if !params.is_null() {
            let bytes: Vec<String> = (0..PARAMS_LOG)
                .map(|b| format!("{:02x}", params.add(b).read()))
                .collect();
            eprintln!("nativ64: PresentX params: {}", bytes.join(""));
        }
    }
    let plane = planes;
    let token = plane.cast::<u64>().read_unaligned();
    let count = plane.add(8).cast::<u32>().read_unaligned();
    let resources = plane.add(0x10).cast::<*mut *mut c_void>().read_unaligned();
    st.frames += 1;
    if st.ring.len() == RING {
        st.ring.pop_front();
    }
    st.ring.push_back((token, Instant::now()));
    if count == 0 || resources.is_null() {
        return S_OK;
    }
    let res = resources.read_unaligned();
    match desc_of(res) {
        Some(d) => {
            if st.frames == 1 || st.frames.is_power_of_two() {
                eprintln!(
                    "nativ64: PresentX frame {} token {token}: {}x{} format {}, {} lists and \
                     {} commands since the last line",
                    st.frames,
                    d.width,
                    d.height,
                    d.format,
                    lists - st.last_lists.min(lists),
                    commands - st.last_commands.min(commands)
                );
                st.last_lists = lists;
                st.last_commands = commands;
            }
        }
        None => {
            if !st.logged_foreign {
                st.logged_foreign = true;
                eprintln!("nativ64: PresentX resource {res:p} is not one of ours");
            }
        }
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::resource::{create_at_address, ResourceDesc, RESOURCE_IIDS};
    use crate::xobj::XObject;

    fn plane(token: u64, count: u32, resources: *mut *mut c_void) -> [u64; 9] {
        let mut p = [0u64; 9];
        p[0] = token;
        p[1] = u64::from(count);
        p[2] = resources as u64;
        p
    }

    /// The state is global, so one test drives it.
    #[test]
    fn counts_frames_and_keeps_the_last_flips() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = ResourceDesc {
                width: 640,
                height: 360,
                ..Default::default()
            };
            let mut r = n;
            assert_eq!(
                create_at_address(n, 0x1000, &desc, 0, n, &RESOURCE_IIDS[0], &mut r),
                S_OK
            );
            let mut res = [r];
            let base = frames();
            let p = plane(7, 1, res.as_mut_ptr());
            assert_eq!(
                present_x(1, p.as_ptr().cast(), std::ptr::null(), 3, 9),
                S_OK
            );
            assert_eq!(frames(), base + 1);
            assert_eq!(recent_tokens().last(), Some(&7));
            // Degenerate calls never fail.
            let none = plane(8, 0, n.cast());
            let null_res = plane(9, 1, std::ptr::null_mut());
            let foreign_obj = [0xdead_beef_u64, 0];
            let mut foreign = [foreign_obj.as_ptr() as *mut c_void];
            let foreign_plane = plane(10, 1, foreign.as_mut_ptr());
            let zero = std::ptr::null();
            assert_eq!(present_x(0, zero, zero, 0, 0), S_OK);
            assert_eq!(present_x(2, zero, zero, 0, 0), S_OK);
            assert_eq!(present_x(1, none.as_ptr().cast(), zero, 0, 0), S_OK);
            assert_eq!(present_x(1, null_res.as_ptr().cast(), zero, 0, 0), S_OK);
            assert_eq!(
                present_x(1, foreign_plane.as_ptr().cast(), zero, 0, 0),
                S_OK
            );
            // The ring keeps the last 16.
            for t in 100..140 {
                let p = plane(t, 1, res.as_mut_ptr());
                present_x(1, p.as_ptr().cast(), zero, 0, 0);
            }
            let tokens = recent_tokens();
            assert_eq!(tokens.len(), RING);
            assert_eq!(tokens.last(), Some(&139));
            assert_eq!(tokens.first(), Some(&124));
            XObject::<crate::d3d12::resource::ResourceData>::release(r);
        }
    }
}
