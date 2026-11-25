//! The device: `D3D12XboxCreateDevice`, the device table and the object layout the title reads.

use std::ffi::c_void;
use std::sync::Mutex;

use crate::xobj::{
    Guid, Iface, SlotTable, XObject, E_NOINTERFACE, E_POINTER, HRESULT, IID_IUNKNOWN, S_OK,
};

pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
/// What D3D returns when the request cannot be served (here: unknown interface version).
pub const DXGI_ERROR_UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;
/// `params[0]` of the title's creation parameters.
pub const INTERFACE_VERSION: u32 = 0x0002_0004;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    Guid::new(d1, d2, d3, d4)
}

/// `ID3D12Device` and the levels the device answers to, as on PC (Device1..Device8, Device11).
pub static DEVICE_IIDS: [Guid; 10] = [
    guid(
        0x189819f1,
        0x1db6,
        0x4b57,
        [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
    ),
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// The console fields of the device, from `+0x08` of the object (0x50 bytes).
#[repr(C)]
pub struct DeviceData {
    /// `+0x08`.
    pub bits: u32,
    /// `+0x0c`: the console's own refcount, unused by the title's inlined code.
    pub console_refs: u32,
    /// `+0x10`: constant-buffer address.
    pub cbuffer_address: u64,
    /// `+0x18`: non-zero makes the title call the vtable instead of writing descriptors inline.
    pub out_of_line: u8,
    _rest: [u8; 0x3f],
}
const _: () = assert!(size_of::<DeviceData>() == 0x50);
const _: () = assert!(std::mem::offset_of!(DeviceData, out_of_line) == 0x10);

static DEVICE_TABLE: SlotTable<160> = SlotTable::new(Iface::Device)
    .with_iunknown::<DeviceData>()
    .with(36, crate::d3d12::fence::create_fence as *const ())
    .with(48, crate::d3d12::queue::create_command_queue_x as *const ())
    .with(
        9,
        crate::d3d12::allocator::create_command_allocator as *const (),
    )
    .with(73, crate::d3d12::list::create_command_list_x as *const ())
    .with(12, crate::d3d12::list::create_command_list as *const ())
    .with(78, crate::d3d12::frame::set_frame_interval as *const ())
    .with(79, crate::d3d12::frame::schedule_frame_event as *const ())
    .with(80, crate::d3d12::frame::wait_frame_event as *const ())
    .with(81, crate::d3d12::frame::get_frame_statistics as *const ())
    .with(
        15,
        crate::d3d12::info::get_descriptor_increment_size as *const (),
    )
    .with(8, crate::d3d12::queue::create_command_queue as *const ())
    .with(
        14,
        crate::d3d12::descriptor::create_descriptor_heap as *const (),
    )
    .with(
        17,
        crate::d3d12::descriptor::create_constant_buffer_view as *const (),
    )
    .with(
        18,
        crate::d3d12::descriptor::create_shader_resource_view as *const (),
    )
    .with(
        19,
        crate::d3d12::descriptor::create_unordered_access_view as *const (),
    )
    .with(
        20,
        crate::d3d12::descriptor::create_render_target_view as *const (),
    )
    .with(
        21,
        crate::d3d12::descriptor::create_depth_stencil_view as *const (),
    )
    .with(22, crate::d3d12::descriptor::create_sampler as *const ())
    .with(23, crate::d3d12::descriptor::copy_descriptors as *const ())
    .with(
        24,
        crate::d3d12::descriptor::copy_descriptors_simple as *const (),
    )
    .with(
        70,
        crate::d3d12::descriptor::create_placed_shader_resource_view as *const (),
    )
    .with(
        71,
        crate::d3d12::descriptor::create_placed_unordered_access_view as *const (),
    )
    .with(
        25,
        crate::d3d12::resource::get_resource_allocation_info as *const (),
    )
    .with(133, crate::d3d12::resource::create_at_address as *const ())
    .with(
        26,
        crate::d3d12::info::get_custom_heap_properties as *const (),
    );

/// The process's device, as an address (the pointer is not `Send`).
static DEVICE: Mutex<usize> = Mutex::new(0);

fn new_device() -> *mut c_void {
    XObject::create(
        &DEVICE_TABLE,
        Iface::Device,
        &DEVICE_IIDS,
        DeviceData {
            bits: 0,
            console_refs: 0,
            cbuffer_address: 0,
            out_of_line: 1,
            _rest: [0; 0x3f],
        },
    )
}

/// The process's device, NULL before creation. No reference is added.
pub fn current() -> *mut c_void {
    *DEVICE.lock().unwrap_or_else(|e| e.into_inner()) as *mut c_void
}

/// `D3D12XboxCreateDevice(adapter, params, iid, out)`.
///
/// # Safety
/// `params` is NULL or points to the creation parameters (at least 4 bytes are read), `iid` is
/// NULL or a GUID, `out` is NULL or writable.
pub unsafe fn create_device(
    _adapter: *mut c_void,
    params: *const u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if params.is_null() {
        return E_INVALIDARG;
    }
    let version = *params;
    if version != INTERFACE_VERSION {
        eprintln!("nativ64: D3D12XboxCreateDevice: interface version {version:#x} not supported");
        return DXGI_ERROR_UNSUPPORTED;
    }
    if iid.is_null() || !(*iid == IID_IUNKNOWN || DEVICE_IIDS.contains(&*iid)) {
        return E_NOINTERFACE;
    }
    let mut slot = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    // The process keeps the creation reference, so the device outlives the title releasing
    // its own and a later call never sees a freed address.
    if *slot == 0 {
        *slot = new_device() as usize;
    }
    let dev = *slot as *mut c_void;
    XObject::<DeviceData>::add_ref(dev);
    *out = dev;
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xobj::query_interface;

    const TITLE_IID: Guid = DEVICE_IIDS[9];
    const PARAMS: [u32; 0x14] = {
        let mut p = [0; 0x14];
        p[0] = INTERFACE_VERSION;
        p
    };

    fn create(iid: &Guid) -> (HRESULT, *mut c_void) {
        let mut out = std::ptr::dangling_mut();
        let hr = unsafe { create_device(std::ptr::null_mut(), PARAMS.as_ptr(), iid, &mut out) };
        (hr, out)
    }

    #[test]
    fn error_cases() {
        unsafe {
            let p = PARAMS.as_ptr();
            let (n, i) = (std::ptr::null_mut(), &TITLE_IID);
            assert_eq!(create_device(n, p, i, n.cast()), E_POINTER);
            let mut out = std::ptr::dangling_mut();
            assert_eq!(
                create_device(n, std::ptr::null(), i, &mut out),
                E_INVALIDARG
            );
            assert!(out.is_null());
            let bad = [0x0002_0005_u32; 0x14];
            assert_eq!(
                create_device(n, bad.as_ptr(), i, &mut out),
                DXGI_ERROR_UNSUPPORTED
            );
            assert!(out.is_null());
            out = std::ptr::dangling_mut();
            assert_eq!(
                create_device(n, p, std::ptr::null(), &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            let other = guid(1, 2, 3, [4; 8]);
            out = std::ptr::dangling_mut();
            assert_eq!(create_device(n, p, &other, &mut out), E_NOINTERFACE);
            assert!(out.is_null());
        }
    }

    #[test]
    fn singleton_refcount_and_layout() {
        let (hr, a) = create(&TITLE_IID);
        assert_eq!(hr, S_OK);
        let (hr, b) = create(&DEVICE_IIDS[0]);
        assert_eq!(hr, S_OK);
        assert_eq!(a, b);
        unsafe {
            // Other tests may hold references too: only relative counts are checked.
            let before = XObject::<DeviceData>::prefix(a).ref_count();
            let mut q = std::ptr::null_mut();
            assert_eq!(
                query_interface::<DeviceData>(a, &DEVICE_IIDS[3], &mut q),
                S_OK
            );
            assert_eq!(q, a);
            assert!(XObject::<DeviceData>::prefix(a).ref_count() > before);
            XObject::<DeviceData>::release(q);

            let raw = a.cast::<u8>();
            assert!((0x08..0x18).all(|o| *raw.add(o) == 0));
            assert_eq!(*raw.add(0x18), 1);
            assert!((0x19..0x58).all(|o| *raw.add(o) == 0));

            let vtbl = *(a as *const *const *const ());
            assert_eq!(*vtbl, query_interface::<DeviceData> as *const ());
            assert_ne!(*vtbl.add(3), *vtbl);
            assert_ne!(*vtbl.add(3), *vtbl.add(1));
            assert_ne!(*vtbl.add(3), *vtbl.add(2));
            assert_ne!(*vtbl.add(3), *vtbl.add(4));

            XObject::<DeviceData>::release(a);
            XObject::<DeviceData>::release(b);
        }
    }
}
