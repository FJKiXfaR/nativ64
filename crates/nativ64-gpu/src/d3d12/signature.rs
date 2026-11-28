//! Command signatures (`ID3D12CommandSignature`), made by `ID3D12Device::CreateCommandSignature`
//! (slot 41, `(desc, root signature, riid, out)`): the record layout of indirect execution, kept
//! for replay. No methods of its own beyond `ID3D12DeviceChild`'s. The root signature is not
//! kept; the arguments are copied.

use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, HRESULT};

/// The command signature at `ptr`, if it is one (the layout of its indirect arguments).
///
/// # Safety
/// `ptr` was recorded from a live object of ours.
pub unsafe fn signature_data<'a>(ptr: usize) -> Option<&'a CommandSignatureData> {
    let obj = ptr as *mut c_void;
    (crate::xobj::XObject::<CommandSignatureData>::prefix(obj).iface()
        == crate::xobj::Iface::CommandSignature)
        .then(|| crate::xobj::XObject::<CommandSignatureData>::data(obj))
}

pub static COMMAND_SIGNATURE_IIDS: [Guid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// `D3D12_COMMAND_SIGNATURE_DESC`.
#[repr(C)]
pub struct CommandSignatureDesc {
    pub stride: u32,
    pub count: u32,
    /// Each `D3D12_INDIRECT_ARGUMENT_DESC`: a type and up to three 32-bit fields.
    pub arguments: *const [u32; 4],
    pub node_mask: u32,
}

#[repr(C)]
pub struct CommandSignatureData {
    bits: u32,
    console_refs: u32,
    pub stride: u32,
    pub arguments: Vec<[u32; 4]>,
}

static TABLE: SlotTable<8> = with_child_methods(
    SlotTable::new(Iface::CommandSignature).with_iunknown::<CommandSignatureData>(),
);

/// `ID3D12Device::CreateCommandSignature(desc, root_signature, riid, out)` (device slot 41).
///
/// # Safety
/// `desc` is NULL or valid with `count` readable arguments; `iid` and `out` NULL or valid.
pub unsafe extern "win64" fn create_command_signature(
    _this: *mut c_void,
    desc: *const CommandSignatureDesc,
    _root: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    let desc = desc
        .as_ref()
        .filter(|d| d.count == 0 || !d.arguments.is_null());
    let Some(desc) = desc else {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    };
    let arguments = if desc.count == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(desc.arguments, desc.count as usize).to_vec()
    };
    hand_out(
        &TABLE,
        Iface::CommandSignature,
        &COMMAND_SIGNATURE_IIDS,
        || CommandSignatureData {
            bits: 0,
            console_refs: 0,
            stride: desc.stride,
            arguments,
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
            let args = [[2u32, 0, 0, 0], [5, 1, 2, 3]];
            let desc = CommandSignatureDesc {
                stride: 32,
                count: 2,
                arguments: args.as_ptr(),
                node_mask: 0,
            };
            let mut s = n;
            let iid = &COMMAND_SIGNATURE_IIDS[0];
            assert_eq!(create_command_signature(n, &desc, n, iid, &mut s), S_OK);
            let d = XObject::<CommandSignatureData>::data(s);
            assert_eq!((d.stride, d.arguments.as_slice()), (32, &args[..]));
            assert_eq!(
                XObject::<CommandSignatureData>::prefix(s).iface(),
                Iface::CommandSignature
            );
            assert_eq!(XObject::<CommandSignatureData>::release(s), 0);
            // Null description; arguments counted but absent.
            s = std::ptr::dangling_mut();
            assert_eq!(
                create_command_signature(n, std::ptr::null(), n, iid, &mut s),
                E_INVALIDARG
            );
            assert!(s.is_null());
            let bad = CommandSignatureDesc {
                stride: 16,
                count: 3,
                arguments: std::ptr::null(),
                node_mask: 0,
            };
            assert_eq!(
                create_command_signature(n, &bad, n, iid, &mut s),
                E_INVALIDARG
            );
            // No output pointer; unknown interface.
            assert_eq!(
                create_command_signature(n, &desc, n, iid, std::ptr::null_mut()),
                S_FALSE
            );
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                create_command_signature(n, &desc, n, &other, &mut s),
                E_NOINTERFACE
            );
        }
    }
}
