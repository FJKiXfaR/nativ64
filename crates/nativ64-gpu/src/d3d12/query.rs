//! Query heaps (`ID3D12QueryHeap`), made by `ID3D12Device::CreateQueryHeap` (slot 39). Nothing
//! produces query results yet; the heap only remembers its description. No methods of its own
//! beyond `ID3D12DeviceChild`'s.

use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, HRESULT};

pub static QUERY_HEAP_IIDS: [Guid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// `D3D12_QUERY_HEAP_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct QueryHeapDesc {
    pub kind: u32,
    pub count: u32,
    pub node_mask: u32,
}

#[repr(C)]
pub struct QueryHeapData {
    bits: u32,
    console_refs: u32,
    pub desc: QueryHeapDesc,
}

static TABLE: SlotTable<8> =
    with_child_methods(SlotTable::new(Iface::QueryHeap).with_iunknown::<QueryHeapData>());

/// `ID3D12Device::CreateQueryHeap(desc, riid, out)` (device slot 39).
///
/// # Safety
/// `desc` is NULL or valid; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_query_heap(
    _this: *mut c_void,
    desc: *const QueryHeapDesc,
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
        Iface::QueryHeap,
        &QUERY_HEAP_IIDS,
        || QueryHeapData {
            bits: 0,
            console_refs: 0,
            desc,
        },
        iid,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{XObject, E_NOINTERFACE, S_OK};

    #[test]
    fn create_and_errors() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = QueryHeapDesc {
                kind: 1,
                count: 64,
                node_mask: 0,
            };
            let mut h = n;
            assert_eq!(
                create_query_heap(n, &desc, &QUERY_HEAP_IIDS[0], &mut h),
                S_OK
            );
            assert_eq!(XObject::<QueryHeapData>::data(h).desc.count, 64);
            assert_eq!(
                XObject::<QueryHeapData>::prefix(h).iface(),
                Iface::QueryHeap
            );
            assert_eq!(XObject::<QueryHeapData>::release(h), 0);
            h = std::ptr::dangling_mut();
            assert_eq!(
                create_query_heap(n, std::ptr::null(), &QUERY_HEAP_IIDS[0], &mut h),
                E_INVALIDARG
            );
            assert!(h.is_null());
            assert_eq!(
                create_query_heap(n, &desc, &QUERY_HEAP_IIDS[0], std::ptr::null_mut()),
                S_FALSE
            );
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(create_query_heap(n, &desc, &other, &mut h), E_NOINTERFACE);
        }
    }
}
