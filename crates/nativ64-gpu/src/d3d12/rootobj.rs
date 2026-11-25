//! Root signature objects (`ID3D12RootSignature`), made by `ID3D12Device::CreateRootSignature`.
//!
//! The object has no methods of its own (only `ID3D12DeviceChild`'s). It keeps the parsed model
//! so `Set*RootSignature` and pipeline creation can read the layout later. The blob must be a
//! DXBC container with an `RTS0` chunk, as our serializer and shader-embedded signatures give.

use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::d3d12::rootsig::{deserialize, RootSignature};
use crate::xobj::{Guid, Iface, SlotTable, HRESULT};

pub static ROOT_SIGNATURE_IIDS: [Guid; 4] = [
    guid(
        0xc54a6b66,
        0x72df,
        0x4ee8,
        [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Console fields from `+0x08`; the model follows (ours, not read by the title).
#[repr(C)]
pub struct RootSignatureData {
    bits: u32,
    console_refs: u32,
    pub model: RootSignature,
}

static TABLE: SlotTable<8> =
    with_child_methods(SlotTable::new(Iface::RootSignature).with_iunknown::<RootSignatureData>());

/// `ID3D12Device::CreateRootSignature(node_mask, blob, size, riid, out)` (device slot 16).
///
/// # Safety
/// `blob` is NULL or readable for `size` bytes; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_root_signature(
    _this: *mut c_void,
    _node_mask: u32,
    blob: *const u8,
    size: usize,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if blob.is_null() || size == 0 {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    }
    let Some(model) = deserialize(std::slice::from_raw_parts(blob, size)) else {
        eprintln!(
            "nativ64: CreateRootSignature: blob is not a DXBC with an RTS0 chunk ({size} bytes)"
        );
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    };
    hand_out(
        &TABLE,
        Iface::RootSignature,
        &ROOT_SIGNATURE_IIDS,
        || RootSignatureData {
            bits: 0,
            console_refs: 0,
            model,
        },
        iid,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::d3d12::rootsig::RootSignature;
    use crate::xobj::{XObject, E_NOINTERFACE, S_OK};

    fn blob() -> Vec<u8> {
        RootSignature {
            version: 1,
            parameters: vec![],
            samplers: vec![],
            flags: 1,
        }
        .serialize()
    }

    #[test]
    fn create_query_release() {
        unsafe {
            let n = std::ptr::null_mut();
            let b = blob();
            let mut r = n;
            assert_eq!(
                create_root_signature(n, 0, b.as_ptr(), b.len(), &ROOT_SIGNATURE_IIDS[0], &mut r),
                S_OK
            );
            let d = XObject::<RootSignatureData>::data(r);
            assert_eq!(d.model.flags, 1);
            assert_eq!(
                XObject::<RootSignatureData>::prefix(r).iface(),
                Iface::RootSignature
            );
            assert_eq!(XObject::<RootSignatureData>::add_ref(r), 2);
            assert_eq!(XObject::<RootSignatureData>::release(r), 1);
            assert_eq!(XObject::<RootSignatureData>::release(r), 0);
        }
    }

    #[test]
    fn error_cases() {
        unsafe {
            let n = std::ptr::null_mut();
            let b = blob();
            let mut r = std::ptr::dangling_mut();
            assert_eq!(
                create_root_signature(n, 0, std::ptr::null(), 4, &ROOT_SIGNATURE_IIDS[0], &mut r),
                E_INVALIDARG
            );
            assert!(r.is_null());
            let junk = [1u8; 40];
            r = std::ptr::dangling_mut();
            assert_eq!(
                create_root_signature(n, 0, junk.as_ptr(), 40, &ROOT_SIGNATURE_IIDS[0], &mut r),
                E_INVALIDARG
            );
            assert!(r.is_null());
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                create_root_signature(n, 0, b.as_ptr(), b.len(), &other, &mut r),
                E_NOINTERFACE
            );
            assert_eq!(
                create_root_signature(n, 0, b.as_ptr(), b.len(), &ROOT_SIGNATURE_IIDS[0], n.cast()),
                S_FALSE
            );
        }
    }
}
