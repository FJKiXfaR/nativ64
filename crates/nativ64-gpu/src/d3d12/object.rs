//! What every console D3D12 object shares: `ID3D12Object` (private data, name),
//! `ID3D12DeviceChild::GetDevice`, and handing a new object to the title.
//!
//! Shortcuts: private data lives in one process-wide map keyed by object address and is not
//! freed when the object dies (a new object at a reused address starts clean, see
//! [`hand_out`]); names only reach the log.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use crate::d3d12::device::{self, E_INVALIDARG};
use crate::xobj::{
    Guid, Iface, SlotTable, XObject, E_NOINTERFACE, E_POINTER, HRESULT, IID_IUNKNOWN, S_OK,
};

/// `S_FALSE`: creation would succeed (the title passed no output pointer).
pub const S_FALSE: HRESULT = 1;
/// `DXGI_ERROR_NOT_FOUND`: private data absent.
pub const NOT_FOUND: HRESULT = 0x887A_0002_u32 as i32;
/// `DXGI_ERROR_MORE_DATA`: the buffer is too small.
pub const MORE_DATA: HRESULT = 0x887A_0003_u32 as i32;

pub const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    Guid::new(d1, d2, d3, d4)
}

/// `ID3D12Pageable`, `ID3D12DeviceChild`, `ID3D12Object`: answered by every child object.
pub const PAGEABLE_IIDS: [Guid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];

type Key = (usize, Guid);
static PRIVATE: Mutex<Option<HashMap<Key, Vec<u8>>>> = Mutex::new(None);

fn with_private<R>(f: impl FnOnce(&mut HashMap<Key, Vec<u8>>) -> R) -> R {
    let mut g = PRIVATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// `ID3D12Object::GetPrivateData(guid, *size, data)` (slot 3).
///
/// # Safety
/// `id` and `size` are NULL or valid; `data` is NULL or has `*size` bytes.
pub unsafe extern "win64" fn get_private_data(
    this: *mut c_void,
    id: *const Guid,
    size: *mut u32,
    data: *mut u8,
) -> HRESULT {
    let (Some(id), Some(size)) = (id.as_ref(), size.as_mut()) else {
        return E_INVALIDARG;
    };
    with_private(|m| {
        let Some(v) = m.get(&(this as usize, *id)) else {
            *size = 0;
            return NOT_FOUND;
        };
        let len = v.len() as u32;
        let room = std::mem::replace(size, len);
        if data.is_null() {
            return S_OK;
        }
        if room < len {
            return MORE_DATA;
        }
        std::ptr::copy_nonoverlapping(v.as_ptr(), data, v.len());
        S_OK
    })
}

/// `ID3D12Object::SetPrivateData(guid, size, data)` (slot 4); NULL data removes the entry.
///
/// # Safety
/// `id` is NULL or valid; `data` is NULL or has `size` bytes.
pub unsafe extern "win64" fn set_private_data(
    this: *mut c_void,
    id: *const Guid,
    size: u32,
    data: *const u8,
) -> HRESULT {
    let Some(&id) = id.as_ref() else {
        return E_INVALIDARG;
    };
    with_private(|m| {
        if data.is_null() {
            m.remove(&(this as usize, id));
        } else {
            m.insert(
                (this as usize, id),
                std::slice::from_raw_parts(data, size as usize).to_vec(),
            );
        }
    });
    S_OK
}

/// `ID3D12Object::SetName(name)` (slot 6): the name only reaches the log.
///
/// # Safety
/// `name` is NULL or a NUL-terminated UTF-16 string.
pub unsafe extern "win64" fn set_name(this: *mut c_void, name: *const u16) -> HRESULT {
    if std::env::var_os("NATIV64_LOG_NAMES").is_some() && !name.is_null() {
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
        eprintln!("nativ64: {this:p} named {s}");
    }
    S_OK
}

/// `ID3D12DeviceChild::GetDevice(riid, out)` (slot 7): the device, as any level it answers to.
///
/// # Safety
/// `iid` is NULL or valid; `out` is NULL or writable.
pub unsafe extern "win64" fn get_device(
    _this: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(iid) = iid.as_ref() else {
        return E_INVALIDARG;
    };
    if *iid != IID_IUNKNOWN && !device::DEVICE_IIDS.contains(iid) {
        return E_NOINTERFACE;
    }
    let dev = device::current();
    if dev.is_null() {
        return E_NOINTERFACE;
    }
    XObject::<device::DeviceData>::add_ref(dev);
    *out = dev;
    S_OK
}

/// Slots 3, 4, 6 (`ID3D12Object`) and 7 (`GetDevice`); slot 5 (`SetPrivateDataInterface`) stays a trap.
pub const fn with_child_methods<const N: usize>(t: SlotTable<N>) -> SlotTable<N> {
    t.with(3, get_private_data as *const ())
        .with(4, set_private_data as *const ())
        .with(6, set_name as *const ())
        .with(7, get_device as *const ())
}

/// Hands a new object to the title as one of `iids` (or `IUnknown`).
///
/// `out` NULL asks only whether creation would work: `S_FALSE`. An unknown IID gives
/// `E_NOINTERFACE` with `*out` NULL. The object is created only on success, so a failed call
/// leaks nothing.
///
/// # Safety
/// `out` is NULL or writable; `iid` is NULL or valid.
pub unsafe fn hand_out<T, const N: usize>(
    table: &'static SlotTable<N>,
    iface: Iface,
    iids: &'static [Guid],
    data: impl FnOnce() -> T,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    let Some(iid) = iid.as_ref() else {
        return E_INVALIDARG;
    };
    if *iid != IID_IUNKNOWN && !iids.contains(iid) {
        return E_NOINTERFACE;
    }
    if out.is_null() {
        return S_FALSE;
    }
    let obj = XObject::create(table, iface, iids, data());
    with_private(|m| m.retain(|k, _| k.0 != obj as usize));
    *out = obj;
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Data(u32);
    static T: SlotTable<8> =
        with_child_methods(SlotTable::new(Iface::Fence).with_iunknown::<Data>());
    static IIDS: [Guid; 1] = [PAGEABLE_IIDS[0]];

    fn make() -> *mut c_void {
        let mut out = std::ptr::null_mut();
        let hr = unsafe { hand_out(&T, Iface::Fence, &IIDS, || Data(0), &IIDS[0], &mut out) };
        assert_eq!(hr, S_OK);
        out
    }

    #[test]
    fn hand_out_cases() {
        unsafe {
            let mut out = std::ptr::dangling_mut();
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(
                hand_out(&T, Iface::Fence, &IIDS, || Data(0), &other, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                hand_out(
                    &T,
                    Iface::Fence,
                    &IIDS,
                    || Data(0),
                    std::ptr::null(),
                    &mut out
                ),
                E_INVALIDARG
            );
            assert_eq!(
                hand_out(
                    &T,
                    Iface::Fence,
                    &IIDS,
                    || Data(0),
                    &IIDS[0],
                    std::ptr::null_mut()
                ),
                S_FALSE
            );
            assert_eq!(
                hand_out(
                    &T,
                    Iface::Fence,
                    &IIDS,
                    || Data(0),
                    &other,
                    std::ptr::null_mut()
                ),
                E_NOINTERFACE
            );
            let o = make();
            assert_eq!(XObject::<Data>::release(o), 0);
        }
    }

    #[test]
    fn private_data_round_trip() {
        unsafe {
            let o = make();
            let id = guid(7, 7, 7, [7; 8]);
            let mut size = 0u32;
            assert_eq!(
                get_private_data(o, &id, &mut size, std::ptr::null_mut()),
                NOT_FOUND
            );
            assert_eq!(set_private_data(o, &id, 3, [1u8, 2, 3].as_ptr()), S_OK);
            assert_eq!(
                get_private_data(o, &id, &mut size, std::ptr::null_mut()),
                S_OK
            );
            assert_eq!(size, 3);
            let mut small = 2u32;
            let mut buf = [0u8; 4];
            assert_eq!(
                get_private_data(o, &id, &mut small, buf.as_mut_ptr()),
                MORE_DATA
            );
            let mut big = 4u32;
            assert_eq!(get_private_data(o, &id, &mut big, buf.as_mut_ptr()), S_OK);
            assert_eq!(buf[..3], [1, 2, 3]);
            assert_eq!(set_private_data(o, &id, 0, std::ptr::null()), S_OK);
            assert_eq!(
                get_private_data(o, &id, &mut size, std::ptr::null_mut()),
                NOT_FOUND
            );
            assert_eq!(
                get_private_data(o, std::ptr::null(), &mut size, std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(
                set_private_data(o, std::ptr::null(), 0, std::ptr::null()),
                E_INVALIDARG
            );
            XObject::<Data>::release(o);
        }
    }

    #[test]
    fn get_device_cases() {
        unsafe {
            let o = make();
            assert_eq!(
                get_device(o, &device::DEVICE_IIDS[0], std::ptr::null_mut()),
                E_POINTER
            );
            let mut dev = std::ptr::dangling_mut();
            assert_eq!(get_device(o, &PAGEABLE_IIDS[0], &mut dev), E_NOINTERFACE);
            assert!(dev.is_null());
            assert_eq!(get_device(o, std::ptr::null(), &mut dev), E_INVALIDARG);
            let mut p = [0u32; 0x14];
            p[0] = device::INTERFACE_VERSION;
            let mut d = std::ptr::null_mut();
            device::create_device(
                std::ptr::null_mut(),
                p.as_ptr(),
                &device::DEVICE_IIDS[0],
                &mut d,
            );
            assert_eq!(get_device(o, &device::DEVICE_IIDS[9], &mut dev), S_OK);
            assert_eq!(dev, d);
            XObject::<device::DeviceData>::release(dev);
            XObject::<device::DeviceData>::release(d);
            XObject::<Data>::release(o);
        }
    }
}
