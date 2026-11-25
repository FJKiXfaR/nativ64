//! `d3d12_xs.dll`: the Xbox flavour of Direct3D 12.

use std::ffi::c_void;

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// Lowest and highest graphics address: all of user space, since GPU addresses are CPU addresses.
const USER_SPACE: (u64, u64) = (0x1_0000, 0x7fff_fffe_ffff);

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in (0x20 bytes).
#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    /// GPU page table size in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across title processes for the frame buffer.
    preserved_bytes: u32,
    _pad: u32,
    /// Out: the range graphics addresses fall in.
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

nativ64::win32! {
    module "d3d12_xs";

    Native fn D3D12ConfigureVirtualMemoryX(config: *mut c_void) -> i32 {
        // SAFETY: the title passes its configuration struct, or null.
        let Some(config) = (unsafe { (config as *mut VirtualMemory).as_mut() }) else {
            return E_INVALIDARG;
        };
        (config.lowest, config.highest) = USER_SPACE;
        S_OK
    }

    Native fn D3D12XboxCreateDevice(adapter: *mut c_void, params: *const u32, iid: *const crate::xobj::Guid, out: *mut *mut c_void) -> i32 {
        // SAFETY: the title passes its own parameters; each pointer is checked inside.
        unsafe { crate::d3d12::device::create_device(adapter, params, iid, out) }
    }

    Native fn D3D12SerializeVersionedRootSignature(desc: *const c_void, blob: *mut *mut c_void, error: *mut *mut c_void) -> i32 {
        // SAFETY: the title passes a description and output pointers; each is checked inside.
        unsafe { crate::d3d12::rootsig::serialize_versioned(desc, blob, error) }
    }

    Native fn D3D12SerializeRootSignature(desc: *const c_void, version: u32, blob: *mut *mut c_void, error: *mut *mut c_void) -> i32 {
        // SAFETY: as above.
        unsafe { crate::d3d12::rootsig::serialize(desc, version, blob, error) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_invalid() {
        assert_eq!(
            unsafe { D3D12ConfigureVirtualMemoryX(std::ptr::null_mut()) },
            E_INVALIDARG
        );
    }

    #[test]
    fn fills_the_range() {
        let mut c = VirtualMemory {
            flags: 1,
            page_table_pages: 2,
            preserved_bytes: 3,
            _pad: 0,
            lowest: 0,
            highest: 0,
        };
        let hr = unsafe { D3D12ConfigureVirtualMemoryX(&mut c as *mut _ as *mut c_void) };
        assert_eq!(hr, S_OK);
        assert_eq!((c.lowest, c.highest), (0x10000, 0x7fff_fffe_ffff));
        assert_eq!((c.flags, c.page_table_pages, c.preserved_bytes), (1, 2, 3));
    }
}
