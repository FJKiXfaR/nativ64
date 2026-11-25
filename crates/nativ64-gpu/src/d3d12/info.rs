//! Device queries answered like the console hardware: unified memory, nothing recorded.

use std::ffi::c_void;

/// `D3D12_HEAP_PROPERTIES` (20 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapProperties {
    pub kind: u32,
    pub cpu_page_property: u32,
    pub memory_pool: u32,
    pub creation_node_mask: u32,
    pub visible_node_mask: u32,
}

const HEAP_TYPE_CUSTOM: u32 = 4;
const CPU_NOT_AVAILABLE: u32 = 1;
const CPU_WRITE_BACK: u32 = 3;
const POOL_L0: u32 = 1;

/// `ID3D12Device::GetCustomHeapProperties(nodeMask, heapType)` (slot 26). The structure is
/// returned through the hidden pointer, which is also the return value. On the console that
/// pointer comes FIRST (`rcx`), `this` second (`rdx`), unlike MSVC on PC (checked with objdump at
/// the title's call site).
///
/// Memory is unified and coherent: every type lives in pool L0; default heaps are not
/// CPU-visible, the others are write-back.
///
/// # Safety
/// `out` is NULL or points to room for a [`HeapProperties`].
pub unsafe extern "win64" fn get_custom_heap_properties(
    out: *mut HeapProperties,
    _this: *mut c_void,
    node_mask: u32,
    heap_type: u32,
) -> *mut HeapProperties {
    if out.is_null() {
        return out;
    }
    let cpu = match heap_type {
        1 => CPU_NOT_AVAILABLE,
        2 | 3 => CPU_WRITE_BACK,
        _ => 0,
    };
    let node = node_mask.max(1);
    *out = HeapProperties {
        kind: HEAP_TYPE_CUSTOM,
        cpu_page_property: cpu,
        memory_pool: if cpu == 0 { 0 } else { POOL_L0 },
        creation_node_mask: node,
        visible_node_mask: node,
    };
    out
}

/// `ID3D12Device::GetDescriptorHandleIncrementSize(type)` (slot 15): views are 32 bytes (RDNA2
/// descriptors), samplers 16, render target and depth views 128 (our own format). An unknown
/// type gives 0, as D3D12 does.
///
/// # Safety
/// Plain call.
pub unsafe extern "win64" fn get_descriptor_increment_size(_this: *mut c_void, kind: u32) -> u32 {
    match kind {
        0 => 32,
        1 => 16,
        2 | 3 => 128,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_sizes() {
        let n = std::ptr::null_mut();
        let f = |k| unsafe { get_descriptor_increment_size(n, k) };
        assert_eq!(
            [f(0), f(1), f(2), f(3), f(4), f(u32::MAX)],
            [32, 16, 128, 128, 0, 0]
        );
    }

    #[test]
    fn heap_properties_by_type() {
        let get = |t, n| unsafe {
            let mut p = HeapProperties::default();
            let r = get_custom_heap_properties(&mut p, std::ptr::null_mut(), n, t);
            assert_eq!(r, &mut p as *mut _);
            p
        };
        let default = get(1, 0);
        assert_eq!(default.kind, 4);
        assert_eq!(default.cpu_page_property, 1);
        assert_eq!(default.memory_pool, 1);
        assert_eq!(default.creation_node_mask, 1);
        let upload = get(2, 1);
        assert_eq!((upload.cpu_page_property, upload.memory_pool), (3, 1));
        assert_eq!(get(3, 1).cpu_page_property, 3);
        assert_eq!(get(0, 1).memory_pool, 0);
    }

    #[test]
    fn null_out_is_tolerated() {
        let r =
            unsafe { get_custom_heap_properties(std::ptr::null_mut(), std::ptr::null_mut(), 1, 1) };
        assert!(r.is_null());
    }
}
