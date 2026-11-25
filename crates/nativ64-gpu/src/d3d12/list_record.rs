//! The command list methods that only record a [`Command`] (no return value, no state to
//! answer from). Each is a slot in the list table; `with_recording` fills them in.
//!
//! Slot numbers are those of the title's layout (see `list.rs`), confirmed by its calls.

use std::ffi::c_void;

use crate::d3d12::command::Command;
use crate::d3d12::list::{record, reset};
use crate::xobj::SlotTable;
use crate::xobj::HRESULT;

/// Pipeline syncs at the top (slot 131, `+0x418`) and bottom (slot 132, `+0x420`) of the pipe.
pub const fn with_recording<const N: usize>(t: SlotTable<N>) -> SlotTable<N> {
    t.with(131, sync_top as *const ())
        .with(132, sync_bottom as *const ())
        .with(139, reset_x as *const ())
}

/// Slot 139 (`+0x458`): `ResetX(allocator, pipeline, flags)`, `Reset` with console flags that only
/// tune the console's command memory.
unsafe extern "win64" fn reset_x(
    this: *mut c_void,
    allocator: *mut c_void,
    pipeline: *mut c_void,
    _flags: u32,
) -> HRESULT {
    reset(this, allocator, pipeline)
}

unsafe fn sync(this: *mut c_void, bottom: bool, flags: u32, control: u32, address: u64, size: u64) {
    record(
        this,
        Command::Sync {
            bottom,
            flags,
            control,
            address,
            size,
        },
    );
}

/// `SyncTop(flags, control, address, size)`.
unsafe extern "win64" fn sync_top(
    this: *mut c_void,
    flags: u32,
    control: u32,
    address: u64,
    size: u64,
) {
    sync(this, false, flags, control, address, size);
}

/// `SyncBottom(flags, control, address, size)`.
unsafe extern "win64" fn sync_bottom(
    this: *mut c_void,
    flags: u32,
    control: u32,
    address: u64,
    size: u64,
) {
    sync(this, true, flags, control, address, size);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::allocator::{create_command_allocator, ALLOCATOR_IIDS};
    use crate::d3d12::list::{create_command_list, recorded, LIST_IIDS};

    #[test]
    fn syncs_are_recorded_through_the_table() {
        unsafe {
            let n = std::ptr::null_mut();
            let mut a = n;
            create_command_allocator(n, 0, &ALLOCATOR_IIDS[0], &mut a);
            let mut l = n;
            assert_eq!(create_command_list(n, 0, 0, a, n, &LIST_IIDS[0], &mut l), 0);
            let vtbl = *(l as *const *const *const ());
            type F = unsafe extern "win64" fn(*mut c_void, u32, u32, u64, u64);
            let bottom: F = std::mem::transmute(*vtbl.add(132));
            let top: F = std::mem::transmute(*vtbl.add(131));
            bottom(l, 0x70_0000, 0, 0, 0xffff_ffff_ff00);
            top(l, 0xc3c0, 0x8002_0000, 0, 0xffff_ffff_ff00);
            assert_eq!(
                recorded(l),
                vec![
                    Command::Sync {
                        bottom: true,
                        flags: 0x70_0000,
                        control: 0,
                        address: 0,
                        size: 0xffff_ffff_ff00
                    },
                    Command::Sync {
                        bottom: false,
                        flags: 0xc3c0,
                        control: 0x8002_0000,
                        address: 0,
                        size: 0xffff_ffff_ff00
                    },
                ]
            );
        }
    }
}
