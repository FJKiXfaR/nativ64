//! Command lists (`ID3D12GraphicsCommandList` and its newer levels).
//!
//! Model: the title's inlined code writes GPU packets into the list's memory while its write
//! pointer is below a limit; here the pointer and every limit are 0, so each method is a
//! vtable call and is recorded as a [`Command`]. The console layout (everything the title's
//! inlined code reads) comes first, our recording state after it. Lists start open, as in
//! D3D12. Shortcut: the allocator is only checked for NULL; recording does not use its memory.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::d3d12::command::Command;
use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::list_record;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, XObject, HRESULT, S_OK};

/// The list interfaces up to `ID3D12GraphicsCommandList6` (ids as on PC, the console's own
/// List5 and List6), then the device child's.
pub static LIST_IIDS: [Guid; 10] = [
    guid(
        0x7116d91c,
        0xe7e4,
        0x47ce,
        [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5],
    ),
    guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    ),
    guid(
        0x553103fb,
        0x1fe7,
        0x4557,
        [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7],
    ),
    guid(
        0x38c3e585,
        0xff17,
        0x412c,
        [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28],
    ),
    guid(
        0x6fda83a7,
        0xb84c,
        0x4e38,
        [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d],
    ),
    guid(
        0x8754318e,
        0xd3a9,
        0x4541,
        [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74],
    ),
    guid(
        0x55050859,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    guid(
        0xc3827890,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Console fields from `+0x08` (the object address is 8 bytes before them).
#[repr(C)]
pub struct ListData {
    bits: u32,
    console_refs: u32,
    /// `+0x10`: the list type.
    pub kind: u32,
    _pad: u32,
    /// `+0x18`: the write pointer, then the plain, draw, API and unbiased API limits. All zero,
    /// so every inline check fails and the title calls the vtable.
    pub put: [u64; 5],
    /// `+0x40`.
    reserved: [u32; 64],
    /// `+0x140`: root signature headers the inlined code keeps for packet building (unused).
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    /// `+0x540`.
    pipeline_shadow: [u32; 8],
    /// `+0x560`: newer SDKs' lists are larger (state flush at `+0x544..+0x564`); room for them.
    newer: [u32; 256],
    // Ours, after the console layout.
    closed: AtomicBool,
    commands: Mutex<Vec<Command>>,
}
const _: () = assert!(std::mem::offset_of!(ListData, kind) == 0x08);
const _: () = assert!(std::mem::offset_of!(ListData, put) == 0x10);
const _: () = assert!(std::mem::offset_of!(ListData, pipeline_shadow) + 32 + 8 == 0x560);

/// The title's layout is the "newer" one of VIctory: every method from the base list's slot 59
/// on sits two slots later than on PC-derived tables (confirmed by the title's pipeline sync
/// calls at `+0x418`/`+0x420`). Slots below 59 are as on PC.
static TABLE: SlotTable<160> = list_record::with_recording(
    with_child_methods(SlotTable::new(Iface::CommandList).with_iunknown::<ListData>())
        .with(8, get_type as *const ())
        .with(9, close as *const ())
        .with(10, reset as *const ()),
);

unsafe fn data<'a>(this: *mut c_void) -> &'a ListData {
    XObject::<ListData>::data(this)
}

/// A new open list recording its initial pipeline binding (if any).
unsafe fn new_list(
    kind: u32,
    allocator: *mut c_void,
    pipeline: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if allocator.is_null() {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    }
    hand_out(
        &TABLE,
        Iface::CommandList,
        &LIST_IIDS,
        || ListData {
            bits: 0,
            console_refs: 0,
            kind,
            _pad: 0,
            put: [0; 5],
            reserved: [0; 64],
            graphics_root_headers: [0; 128],
            compute_root_headers: [0; 128],
            pipeline_shadow: [0; 8],
            newer: [0; 256],
            closed: AtomicBool::new(false),
            commands: Mutex::new(initial(pipeline)),
        },
        iid,
        out,
    )
}

fn initial(pipeline: *mut c_void) -> Vec<Command> {
    if pipeline.is_null() {
        Vec::new()
    } else {
        vec![Command::SetPipeline {
            pipeline: pipeline as usize,
        }]
    }
}

/// `ID3D12Device::CreateCommandList(nodeMask, type, allocator, pipeline, riid, out)` (slot 12).
///
/// # Safety
/// Pointers are NULL or valid objects of ours.
pub unsafe extern "win64" fn create_command_list(
    _this: *mut c_void,
    _node_mask: u32,
    kind: u32,
    allocator: *mut c_void,
    pipeline: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    new_list(kind, allocator, pipeline, iid, out)
}

/// The console's list description (`CreateCommandListX`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ListDesc {
    pub kind: u32,
    pub flags: u32,
    pub node_mask: u32,
    pub barrier_batch: u32,
}

/// `ID3D12Device::CreateCommandListX(desc, allocator, pipeline, riid, out)` (slot 73).
///
/// # Safety
/// Pointers are NULL or valid.
pub unsafe extern "win64" fn create_command_list_x(
    _this: *mut c_void,
    desc: *const ListDesc,
    allocator: *mut c_void,
    pipeline: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(desc) = desc.as_ref() else {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    };
    new_list(desc.kind, allocator, pipeline, iid, out)
}

/// Slot 8: `ID3D12CommandList::GetType`.
unsafe extern "win64" fn get_type(this: *mut c_void) -> u32 {
    data(this).kind
}

/// Slot 9: `Close`.
unsafe extern "win64" fn close(this: *mut c_void) -> HRESULT {
    data(this).closed.store(true, Ordering::Release);
    S_OK
}

/// Slot 10: `Reset(allocator, pipeline)`: drops the recording and reopens the list.
pub(crate) unsafe extern "win64" fn reset(
    this: *mut c_void,
    allocator: *mut c_void,
    pipeline: *mut c_void,
) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    let d = data(this);
    *d.commands.lock().unwrap_or_else(|e| e.into_inner()) = initial(pipeline);
    d.closed.store(false, Ordering::Release);
    S_OK
}

/// What a list recorded so far (for backends and tests).
///
/// # Safety
/// `this` is a live list of ours.
pub unsafe fn recorded(this: *mut c_void) -> Vec<Command> {
    data(this)
        .commands
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Appends a command (used by the recording methods).
///
/// # Safety
/// `this` is a live list of ours.
pub unsafe fn record(this: *mut c_void, command: Command) {
    data(this)
        .commands
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(command);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::allocator::{create_command_allocator, AllocatorData, ALLOCATOR_IIDS};
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::E_NOINTERFACE;

    type Slot1 = unsafe extern "win64" fn(*mut c_void) -> u32;

    unsafe fn alloc() -> *mut c_void {
        let mut a = std::ptr::null_mut();
        create_command_allocator(std::ptr::null_mut(), 0, &ALLOCATOR_IIDS[0], &mut a);
        a
    }

    #[test]
    fn creation_layout_and_methods() {
        unsafe {
            let n = std::ptr::null_mut();
            let a = alloc();
            let desc = ListDesc {
                kind: 3,
                ..Default::default()
            };
            let pipeline = 0x1000 as *mut c_void;
            let mut l = n;
            assert_eq!(
                create_command_list_x(n, &desc, a, pipeline, &LIST_IIDS[1], &mut l),
                S_OK
            );
            let raw = l.cast::<u8>();
            assert_eq!(*raw.add(0x10).cast::<u32>(), 3);
            assert!(
                (0x18..0x40).all(|o| *raw.add(o) == 0),
                "write pointer and limits are 0"
            );
            assert_eq!(recorded(l), vec![Command::SetPipeline { pipeline: 0x1000 }]);
            let vtbl = *(l as *const *const *const ());
            let ty: Slot1 = std::mem::transmute(*vtbl.add(8));
            assert_eq!(ty(l), 3);
            let close: unsafe extern "win64" fn(*mut c_void) -> HRESULT =
                std::mem::transmute(*vtbl.add(9));
            assert_eq!(close(l), S_OK);
            assert!(data(l).closed.load(Ordering::Acquire));
            let reset: unsafe extern "win64" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT =
                std::mem::transmute(*vtbl.add(10));
            assert_eq!(reset(l, n, n), E_INVALIDARG);
            assert_eq!(reset(l, a, n), S_OK);
            assert!(recorded(l).is_empty());
            assert!(!data(l).closed.load(Ordering::Acquire));
            assert_eq!(XObject::<ListData>::release(l), 0);
            assert_eq!(XObject::<AllocatorData>::release(a), 0);
        }
    }

    #[test]
    fn error_cases() {
        unsafe {
            let n = std::ptr::null_mut();
            let a = alloc();
            let desc = ListDesc::default();
            let mut l = std::ptr::dangling_mut();
            assert_eq!(
                create_command_list_x(n, &desc, n, n, &LIST_IIDS[0], &mut l),
                E_INVALIDARG
            );
            assert!(l.is_null());
            assert_eq!(
                create_command_list_x(n, std::ptr::null(), a, n, &LIST_IIDS[0], &mut l),
                E_INVALIDARG
            );
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                create_command_list(n, 0, 0, a, n, &other, &mut l),
                E_NOINTERFACE
            );
            assert_eq!(
                create_command_list(n, 0, 0, a, n, &LIST_IIDS[0], n.cast()),
                S_FALSE
            );
            assert_eq!(
                create_command_list(n, 0, 0, a, n, &LIST_IIDS[7], &mut l),
                S_OK
            );
            assert_eq!(XObject::<ListData>::release(l), 0);
            XObject::<AllocatorData>::release(a);
        }
    }
}
