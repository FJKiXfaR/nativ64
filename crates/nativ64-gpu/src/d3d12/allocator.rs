//! Command allocators (`ID3D12CommandAllocator`).
//!
//! Model: lists record into our own `Command` stream, not into allocator memory, so an
//! allocator only remembers its list type and `Reset` has nothing to free.

use std::ffi::c_void;

use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, HRESULT, S_OK};

pub static ALLOCATOR_IIDS: [Guid; 4] = [
    guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Console fields from `+0x08`; the list type is at `+0x10`.
#[repr(C)]
pub struct AllocatorData {
    bits: u32,
    console_refs: u32,
    pub kind: u32,
}

static TABLE: SlotTable<9> =
    with_child_methods(SlotTable::new(Iface::CommandAllocator).with_iunknown::<AllocatorData>())
        .with(8, reset as *const ());

/// `ID3D12Device::CreateCommandAllocator(type, riid, out)` (device slot 9).
///
/// # Safety
/// `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_command_allocator(
    _this: *mut c_void,
    kind: u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    hand_out(
        &TABLE,
        Iface::CommandAllocator,
        &ALLOCATOR_IIDS,
        || AllocatorData {
            bits: 0,
            console_refs: 0,
            kind,
        },
        iid,
        out,
    )
}

/// Slot 8.
unsafe extern "win64" fn reset(_this: *mut c_void) -> HRESULT {
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{XObject, E_NOINTERFACE};

    #[test]
    fn create_reset_and_layout() {
        unsafe {
            let n = std::ptr::null_mut();
            let mut a = n;
            assert_eq!(
                create_command_allocator(n, 2, &ALLOCATOR_IIDS[0], &mut a),
                S_OK
            );
            assert_eq!(*a.cast::<u8>().add(0x10).cast::<u32>(), 2);
            let vtbl = *(a as *const *const *const ());
            let f: unsafe extern "win64" fn(*mut c_void) -> HRESULT =
                std::mem::transmute(*vtbl.add(8));
            assert_eq!(f(a), S_OK);
            assert_eq!(XObject::<AllocatorData>::release(a), 0);
        }
    }

    #[test]
    fn error_cases() {
        unsafe {
            let n = std::ptr::null_mut();
            let mut a = std::ptr::dangling_mut();
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                create_command_allocator(n, 0, &other, &mut a),
                E_NOINTERFACE
            );
            assert!(a.is_null());
            assert_eq!(
                create_command_allocator(n, 0, std::ptr::null(), &mut a),
                crate::d3d12::device::E_INVALIDARG
            );
            assert_eq!(
                create_command_allocator(n, 0, &ALLOCATOR_IIDS[0], n.cast()),
                S_FALSE
            );
        }
    }
}
