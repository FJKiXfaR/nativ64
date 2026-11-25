//! Command queues (`ID3D12CommandQueue`).
//!
//! Model: a queue records its description; submissions are recorded work for the backend (none
//! yet). The slot layout depends on the title's SDK (see docs/gpu.md): only slots confirmed by
//! a run are filled, the others trap.

use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, HRESULT};

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

static TABLE: SlotTable<36> =
    with_child_methods(SlotTable::new(Iface::CommandQueue).with_iunknown::<QueueData>());

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
