//! Command queues (`ID3D12CommandQueue`).
//!
//! Model: a queue records its description and counts what is submitted. No backend replays the
//! recorded command streams yet, so a submission only counts lists and commands, and GPU work
//! completes at once: `Signal` moves its fence immediately. The slot layout is the "newer" one
//! (every slot from 14 on is two later than the older SDK's, see docs/gpu.md). `PresentX` (32) is
//! in `present.rs`; the page mapping batch (31) is not filled: it traps.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::d3d12::command::Command;
use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::d3d12::{fence, list, present};
use crate::xobj::{Guid, Iface, SlotTable, XObject, HRESULT, S_OK};

/// `D3D12_COMMAND_QUEUE_DESC` (PC form).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDesc {
    pub kind: u32,
    pub priority: i32,
    pub flags: u32,
    pub node_mask: u32,
}

/// Console fields, from `+0x08`: the title reads the list type at `+0x10`.
#[repr(C)]
pub struct QueueData {
    bits: u32,
    console_refs: u32,
    pub kind: u32,
    pub desc: QueueDesc,
    /// Lists and commands handed to `ExecuteCommandLists[X]` so far.
    submitted_lists: AtomicU64,
    submitted_commands: AtomicU64,
}
const _: () = assert!(std::mem::offset_of!(QueueData, kind) == 0x08);

pub static QUEUE_IIDS: [Guid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Ticks per second of GPU timestamps: the console's 100 MHz clock (VIctory's choice; xcl's
/// null device says 10 MHz, which only matters to the title's own conversions).
pub const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

static TABLE: SlotTable<36> =
    with_child_methods(SlotTable::new(Iface::CommandQueue).with_iunknown::<QueueData>())
        .with(10, execute_command_lists as *const ())
        .with(11, marker as *const ())
        .with(12, marker as *const ())
        .with(13, marker as *const ())
        .with(16, signal as *const ())
        .with(17, wait as *const ())
        .with(18, get_timestamp_frequency as *const ())
        .with(28, get_desc as *const ())
        .with(32, present_x as *const ())
        .with(34, signal_x as *const ())
        .with(35, execute_command_lists_x as *const ());

unsafe fn data<'a>(this: *mut c_void) -> &'a QueueData {
    XObject::<QueueData>::data(this)
}

/// Slot 10, `ExecuteCommandLists(count, lists)`: counts the lists and their recorded commands
/// (no backend replays them yet); the work is complete on return.
unsafe extern "win64" fn execute_command_lists(
    this: *mut c_void,
    count: u32,
    lists: *const *mut c_void,
) {
    if this.is_null() || lists.is_null() {
        return;
    }
    let q = data(this);
    for &l in std::slice::from_raw_parts(lists, count as usize) {
        if l.is_null() {
            continue;
        }
        let commands: Vec<Command> = list::recorded(l);
        q.submitted_lists.fetch_add(1, Ordering::Relaxed);
        q.submitted_commands
            .fetch_add(commands.len() as u64, Ordering::Relaxed);
    }
}

/// Slot 35, `ExecuteCommandListsX(count, lists, frame token, flags)`: frames are not paced, so
/// it is a plain submission.
unsafe extern "win64" fn execute_command_lists_x(
    this: *mut c_void,
    count: u32,
    lists: *const *mut c_void,
    _token: u64,
    _flags: u32,
) {
    execute_command_lists(this, count, lists);
}

/// Slots 11 to 13: `SetMarker`, `BeginEvent`, `EndEvent` only matter to profilers.
unsafe extern "win64" fn marker(_this: *mut c_void) {}

/// Slot 16, `Signal(fence, value)`: earlier work is complete, so the fence moves now.
unsafe extern "win64" fn signal(_this: *mut c_void, fence: *mut c_void, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    fence::set(fence, value);
    S_OK
}

/// Slot 34, the console `Signal` with sync settings for the bottom and top of the pipe.
unsafe extern "win64" fn signal_x(
    this: *mut c_void,
    fence: *mut c_void,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}

/// Slot 32, `PresentX(plane_count, planes, params)`: see `present.rs`.
unsafe extern "win64" fn present_x(
    this: *mut c_void,
    plane_count: u32,
    planes: *const u8,
    params: *const u8,
) -> HRESULT {
    let (lists, commands) = if this.is_null() {
        (0, 0)
    } else {
        let q = data(this);
        (
            q.submitted_lists.load(Ordering::Relaxed),
            q.submitted_commands.load(Ordering::Relaxed),
        )
    };
    present::present_x(plane_count, planes, params, lists, commands)
}

/// Slot 17, `Wait(fence, value)`: all signals have already run, so nothing can be pending.
///
/// shortcut: a fence value no signal reached yet does not block the queue.
unsafe extern "win64" fn wait(_this: *mut c_void, fence: *mut c_void, _value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    S_OK
}

/// Slot 18, `GetTimestampFrequency(out)`.
unsafe extern "win64" fn get_timestamp_frequency(_this: *mut c_void, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    out.write(TIMESTAMP_FREQUENCY);
    S_OK
}

/// Slot 28, `GetDesc()`: a 16-byte struct, so the return pointer comes first and `this` second.
unsafe extern "win64" fn get_desc(out: *mut QueueDesc, this: *mut c_void) -> *mut QueueDesc {
    if !out.is_null() {
        out.write(data(this).desc);
    }
    out
}

/// `ID3D12Device::CreateCommandQueue(desc, riid, out)` (device slot 8).
///
/// # Safety
/// `desc` is NULL or a description; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_command_queue(
    _this: *mut c_void,
    desc: *const QueueDesc,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(&desc) = desc.as_ref() else {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    };
    hand_out(
        &TABLE,
        Iface::CommandQueue,
        &QUEUE_IIDS,
        || QueueData {
            bits: 0,
            console_refs: 0,
            kind: desc.kind,
            desc,
            submitted_lists: AtomicU64::new(0),
            submitted_commands: AtomicU64::new(0),
        },
        iid,
        out,
    )
}

/// The console's queue description: an engine or pipe and a queue on it instead of PC's
/// priority and node mask.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDescX {
    pub kind: u32,
    pub flags: u32,
    pub engine_or_pipe: u32,
    pub queue: u32,
}

/// `ID3D12Device::CreateCommandQueueX(desc, riid, out)` (device slot 48). Shortcut: the
/// engine, pipe and queue are ignored; the queue is a plain one of that kind.
///
/// # Safety
/// `desc` is NULL or a description; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_command_queue_x(
    this: *mut c_void,
    desc: *const QueueDescX,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(x) = desc.as_ref() else {
        return create_command_queue(this, std::ptr::null(), iid, out);
    };
    let pc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        node_mask: 1,
    };
    create_command_queue(this, &pc, iid, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{XObject, E_NOINTERFACE, S_OK};

    #[test]
    fn create_and_layout() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = QueueDesc {
                kind: 2,
                ..Default::default()
            };
            let mut q = n;
            assert_eq!(create_command_queue(n, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
            assert_eq!(*q.cast::<u8>().add(0x10).cast::<u32>(), 2);
            assert_eq!(XObject::<QueueData>::release(q), 0);
            let x = QueueDescX {
                kind: 4,
                engine_or_pipe: 1,
                ..Default::default()
            };
            assert_eq!(create_command_queue_x(n, &x, &QUEUE_IIDS[0], &mut q), S_OK);
            assert_eq!(*q.cast::<u8>().add(0x10).cast::<u32>(), 4);
            assert_eq!(XObject::<QueueData>::release(q), 0);
            assert_eq!(
                create_command_queue_x(n, std::ptr::null(), &QUEUE_IIDS[0], &mut q),
                E_INVALIDARG
            );
            assert!(q.is_null());
        }
    }

    unsafe fn slot(q: *mut c_void, n: usize) -> *const () {
        *(*q.cast::<*const *const ()>()).add(n)
    }

    unsafe fn queue() -> *mut c_void {
        let mut q = std::ptr::null_mut();
        let desc = QueueDesc {
            kind: 2,
            priority: 7,
            flags: 1,
            node_mask: 1,
        };
        assert_eq!(
            create_command_queue(std::ptr::null_mut(), &desc, &QUEUE_IIDS[0], &mut q),
            S_OK
        );
        q
    }

    #[test]
    fn signal_wait_and_markers() {
        use crate::d3d12::fence::{create_fence, FENCE_IIDS};
        type Sig = unsafe extern "win64" fn(*mut c_void, *mut c_void, u64) -> HRESULT;
        type SigX = unsafe extern "win64" fn(
            *mut c_void,
            *mut c_void,
            u64,
            *const u8,
            *const u8,
        ) -> HRESULT;
        type Mark = unsafe extern "win64" fn(*mut c_void);
        unsafe {
            let q = queue();
            let mut f = std::ptr::null_mut();
            create_fence(q, 0, 0, &FENCE_IIDS[0], &mut f);
            let completed = |f: *mut c_void| {
                let g: unsafe extern "win64" fn(*mut c_void) -> u64 =
                    std::mem::transmute(slot(f, 8));
                g(f)
            };
            let sig: Sig = std::mem::transmute(slot(q, 16));
            assert_eq!(sig(q, f, 5), S_OK);
            assert_eq!(completed(f), 5);
            let sigx: SigX = std::mem::transmute(slot(q, 34));
            let n = std::ptr::null();
            assert_eq!(sigx(q, f, 9, n, n), S_OK);
            assert_eq!(completed(f), 9);
            let wait: Sig = std::mem::transmute(slot(q, 17));
            assert_eq!(wait(q, f, 100), S_OK);
            let n = std::ptr::null_mut();
            assert_eq!(sig(q, n, 1), E_INVALIDARG);
            assert_eq!(
                sigx(q, n, 1, std::ptr::null(), std::ptr::null()),
                E_INVALIDARG
            );
            assert_eq!(wait(q, n, 1), E_INVALIDARG);
            for s in 11..=13 {
                let m: Mark = std::mem::transmute(slot(q, s));
                m(q);
            }
            XObject::<crate::d3d12::fence::FenceData>::release(f);
            XObject::<QueueData>::release(q);
        }
    }

    #[test]
    fn frequency_and_desc() {
        type Freq = unsafe extern "win64" fn(*mut c_void, *mut u64) -> HRESULT;
        type Desc = unsafe extern "win64" fn(*mut QueueDesc, *mut c_void) -> *mut QueueDesc;
        unsafe {
            let q = queue();
            let freq: Freq = std::mem::transmute(slot(q, 18));
            let mut hz = 0;
            assert_eq!(freq(q, &mut hz), S_OK);
            assert_eq!(hz, TIMESTAMP_FREQUENCY);
            assert_eq!(freq(q, std::ptr::null_mut()), E_INVALIDARG);
            let desc: Desc = std::mem::transmute(slot(q, 28));
            let mut d = QueueDesc::default();
            assert_eq!(desc(&mut d, q), &mut d as *mut _);
            assert_eq!((d.kind, d.priority, d.flags, d.node_mask), (2, 7, 1, 1));
            assert!(desc(std::ptr::null_mut(), q).is_null());
            XObject::<QueueData>::release(q);
        }
    }

    #[test]
    fn execute_counts_lists() {
        use crate::d3d12::allocator::{create_command_allocator, ALLOCATOR_IIDS};
        use crate::d3d12::list::{create_command_list, LIST_IIDS};
        type Exec = unsafe extern "win64" fn(*mut c_void, u32, *const *mut c_void);
        type ExecX = unsafe extern "win64" fn(*mut c_void, u32, *const *mut c_void, u64, u32);
        unsafe {
            let q = queue();
            let n = std::ptr::null_mut();
            let mut a = n;
            create_command_allocator(n, 0, &ALLOCATOR_IIDS[0], &mut a);
            let mut l = n;
            create_command_list(n, 0, 0, a, n, &LIST_IIDS[0], &mut l);
            let exec: Exec = std::mem::transmute(slot(q, 10));
            let lists = [l, n];
            exec(q, 2, lists.as_ptr());
            exec(q, 1, std::ptr::null());
            let d = data(q);
            assert_eq!(d.submitted_lists.load(Ordering::Relaxed), 1);
            assert_eq!(
                d.submitted_commands.load(Ordering::Relaxed),
                list::recorded(l).len() as u64
            );
            let x: ExecX = std::mem::transmute(slot(q, 35));
            x(q, 1, lists.as_ptr(), 0, 0);
            assert_eq!(d.submitted_lists.load(Ordering::Relaxed), 2);
            XObject::<QueueData>::release(q);
        }
    }

    #[test]
    fn present_x_is_filled() {
        type Present = unsafe extern "win64" fn(*mut c_void, u32, *const u8, *const u8) -> HRESULT;
        unsafe {
            let q = queue();
            assert_eq!(slot(q, 32), present_x as *const ());
            let p: Present = std::mem::transmute(slot(q, 32));
            let n = std::ptr::null();
            assert_eq!(p(q, 0, n, n), S_OK);
            assert_eq!(p(q, 1, n, n), S_OK);
            XObject::<QueueData>::release(q);
        }
    }

    #[test]
    fn error_cases() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = QueueDesc::default();
            let mut q = std::ptr::dangling_mut();
            assert_eq!(
                create_command_queue(n, std::ptr::null(), &QUEUE_IIDS[0], &mut q),
                E_INVALIDARG
            );
            assert!(q.is_null());
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                create_command_queue(n, &desc, &other, &mut q),
                E_NOINTERFACE
            );
            assert_eq!(
                create_command_queue(n, &desc, &QUEUE_IIDS[0], n.cast()),
                S_FALSE
            );
        }
    }
}
