//! Fences (`ID3D12Fence`, `ID3D12Fence1`).
//!
//! Model: a 64-bit value in the object (its address is the fence's GPU address, which the title
//! may read straight from memory) and the events to set when it reaches their value. With the
//! null backend, queue signals complete at once, so [`set`] is all a queue needs.
//! Shortcut: `SetEventOnCompletion` with a NULL event and an unreached value fails with
//! `E_NOTIMPL` instead of blocking (nothing could ever reach the value while no GPU work runs).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, XObject, HRESULT, S_OK};
use nativ64::com::E_NOTIMPL;

pub static FENCE_IIDS: [Guid; 5] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    // ID3D12Fence1.
    guid(
        0x433685fe,
        0xe22b,
        0x4ca0,
        [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Console fields from `+0x08`.
#[repr(C)]
pub struct FenceData {
    bits: u32,
    console_refs: u32,
    /// `+0x10`.
    value: AtomicU64,
    flags: u32,
    /// (value to reach, event handle).
    waiters: Mutex<Vec<(u64, usize)>>,
}

static TABLE: SlotTable<24> =
    with_child_methods(SlotTable::new(Iface::Fence).with_iunknown::<FenceData>())
        .with(8, get_completed_value as *const ())
        .with(9, set_event_on_completion as *const ())
        .with(10, signal as *const ())
        .with(11, get_creation_flags as *const ())
        .with(12, get_gpu_address as *const ());

/// `ID3D12Device::CreateFence(initial, flags, riid, out)` (device slot 36).
///
/// # Safety
/// `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_fence(
    _this: *mut c_void,
    initial: u64,
    flags: u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    hand_out(
        &TABLE,
        Iface::Fence,
        &FENCE_IIDS,
        || FenceData {
            bits: 0,
            console_refs: 0,
            value: AtomicU64::new(initial),
            flags,
            waiters: Mutex::new(Vec::new()),
        },
        iid,
        out,
    )
}

unsafe fn data<'a>(this: *mut c_void) -> &'a FenceData {
    XObject::<FenceData>::data(this)
}

/// Moves the fence to `value` and sets the events waiting for it (what a queue's `Signal` does
/// once its earlier work ran).
///
/// # Safety
/// `this` is a live fence.
pub unsafe fn set(this: *mut c_void, value: u64) {
    let f = data(this);
    let mut waiters = f.waiters.lock().unwrap_or_else(|e| e.into_inner());
    f.value.store(value, Ordering::Release);
    waiters.retain(|&(wanted, event)| {
        if wanted <= value {
            nativ64::handles::set_event(event);
            false
        } else {
            true
        }
    });
}

/// Slot 8.
unsafe extern "win64" fn get_completed_value(this: *mut c_void) -> u64 {
    data(this).value.load(Ordering::Acquire)
}

/// Slot 9.
unsafe extern "win64" fn set_event_on_completion(
    this: *mut c_void,
    value: u64,
    event: *mut c_void,
) -> HRESULT {
    let f = data(this);
    let mut waiters = f.waiters.lock().unwrap_or_else(|e| e.into_inner());
    if f.value.load(Ordering::Acquire) >= value {
        if !event.is_null() {
            nativ64::handles::set_event(event as usize);
        }
        return S_OK;
    }
    if event.is_null() {
        return E_NOTIMPL;
    }
    waiters.push((value, event as usize));
    S_OK
}

/// Slot 10: the CPU's own `Signal`.
unsafe extern "win64" fn signal(this: *mut c_void, value: u64) -> HRESULT {
    set(this, value);
    S_OK
}

/// Slot 11.
unsafe extern "win64" fn get_creation_flags(this: *mut c_void) -> u32 {
    data(this).flags
}

/// Slot 12: the value's address.
unsafe extern "win64" fn get_gpu_address(this: *mut c_void) -> u64 {
    data(this).value.as_ptr() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xobj::E_NOINTERFACE;

    fn make(initial: u64, flags: u32) -> *mut c_void {
        let mut f = std::ptr::null_mut();
        let hr =
            unsafe { create_fence(std::ptr::null_mut(), initial, flags, &FENCE_IIDS[0], &mut f) };
        assert_eq!(hr, S_OK);
        f
    }

    #[test]
    fn value_flags_address_and_layout() {
        unsafe {
            let f = make(3, 5);
            assert_eq!(get_completed_value(f), 3);
            assert_eq!(get_creation_flags(f), 5);
            assert_eq!(signal(f, 9), S_OK);
            let at = get_gpu_address(f) as *const u64;
            assert_eq!(*at, 9);
            assert_eq!(at as usize, f as usize + 0x10);
            assert_eq!(XObject::<FenceData>::release(f), 0);
        }
    }

    #[test]
    fn events_and_errors() {
        unsafe {
            let f = make(1, 0);
            assert_eq!(set_event_on_completion(f, 1, std::ptr::null_mut()), S_OK);
            assert_eq!(
                set_event_on_completion(f, 4, std::ptr::null_mut()),
                E_NOTIMPL
            );
            // An unreached wait is queued and woken by a later signal (handle 0 is not an event:
            // set_event just fails, the entry still leaves the list).
            assert_eq!(
                set_event_on_completion(f, 4, 0x7fff_0001 as *mut c_void),
                S_OK
            );
            assert_eq!(data(f).waiters.lock().unwrap().len(), 1);
            set(f, 4);
            assert!(data(f).waiters.lock().unwrap().is_empty());
            let other = guid(1, 2, 3, [4; 8]);
            let mut out = std::ptr::dangling_mut();
            assert_eq!(
                create_fence(std::ptr::null_mut(), 0, 0, &other, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                create_fence(std::ptr::null_mut(), 0, 0, &FENCE_IIDS[1], &mut out),
                S_OK
            );
            XObject::<FenceData>::release(out);
            XObject::<FenceData>::release(f);
        }
    }
}
