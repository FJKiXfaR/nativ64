//! `ID3DBlob` (`ID3D10Blob`): a plain COM object holding bytes, as `D3DCompile` and the root
//! signature serializers return. Not an Xbox-layout object: the title only calls its methods.

use std::ffi::c_void;

use nativ64::com::{ComObject, Guid, IUnknownVtbl};

/// `{8ba5fb08-5195-40e2-ac58-0d989c3a0102}`: `ID3DBlob`, the same interface as `ID3D10Blob`.
pub const IID_BLOB: Guid = Guid::new(
    0x8ba5_fb08,
    0x5195,
    0x40e2,
    [0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02],
);
static BLOB_IIDS: [Guid; 1] = [IID_BLOB];

type BlobObj = ComObject<BlobVtbl, Vec<u8>>;

#[repr(C)]
pub struct BlobVtbl {
    unknown: IUnknownVtbl<BlobVtbl, Vec<u8>>,
    get_buffer_pointer: unsafe extern "win64" fn(*mut BlobObj) -> *mut c_void,
    get_buffer_size: unsafe extern "win64" fn(*mut BlobObj) -> usize,
}

/// Slot 3.
unsafe extern "win64" fn get_buffer_pointer(this: *mut BlobObj) -> *mut c_void {
    (*this).data.as_mut_ptr().cast()
}

/// Slot 4.
unsafe extern "win64" fn get_buffer_size(this: *mut BlobObj) -> usize {
    (*this).data.len()
}

static BLOB_VTBL: BlobVtbl = BlobVtbl {
    unknown: IUnknownVtbl::new(),
    get_buffer_pointer,
    get_buffer_size,
};

/// A new blob with one reference, to hand to the title (it releases it).
pub fn create(bytes: Vec<u8>) -> *mut c_void {
    BlobObj::create(&BLOB_VTBL, &BLOB_IIDS, bytes).cast()
}

/// The bytes of a blob made by [`create`].
///
/// # Safety
/// `blob` is a live object returned by [`create`], and the slice is not used after its release.
pub unsafe fn bytes<'a>(blob: *mut c_void) -> &'a [u8] {
    &(*(blob as *mut BlobObj)).data
}

#[cfg(test)]
mod tests {
    use super::*;
    use nativ64::com::{add_ref, release};

    #[test]
    fn methods_and_refcount() {
        let b = create(vec![1, 2, 3]) as *mut BlobObj;
        unsafe {
            assert_eq!(get_buffer_size(b), 3);
            let p = get_buffer_pointer(b) as *const u8;
            assert_eq!(std::slice::from_raw_parts(p, 3), &[1, 2, 3]);
            assert_eq!(add_ref::<BlobVtbl, Vec<u8>>(b), 2);
            assert_eq!(release::<BlobVtbl, Vec<u8>>(b), 1);
            assert_eq!(release::<BlobVtbl, Vec<u8>>(b), 0);
        }
    }
}
