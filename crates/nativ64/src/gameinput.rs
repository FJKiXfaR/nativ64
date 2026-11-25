//! `GameInput.dll`: the console input API (`IGameInput`, v0), with no devices yet.
//!
//! The DLL's single export is ordinal 1, `GameInputCreate(IGameInput **)`. The object has the 22
//! vtable slots of the v0 interface in published order (`IUnknown`, then GetCurrentTimestamp,
//! GetCurrentReading, GetNextReading, GetPreviousReading, GetTemporalReading,
//! RegisterReadingCallback, RegisterDeviceCallback, RegisterGuideButtonCallback,
//! RegisterKeyboardLayoutCallback, StopCallback, UnregisterCallback, CreateDispatcher,
//! CreateAggregateDevice, FindDeviceFromId, FindDeviceFromObject, FindDeviceFromPlatformHandle,
//! FindDeviceFromPlatformString, EnableOemDeviceSupport, SetFocusPolicy). The slot order and
//! IID come from the public GDK header as read through another loader's table; no code is shared.
//!
//! Behaviour: no device exists, so every reading query is `GAMEINPUT_E_READING_NOT_FOUND` and
//! lookups of a device report `GAMEINPUT_E_DEVICE_NOT_FOUND`. Registrations are stored and
//! return a token but never fire. TODO: feed devices and readings from evdev, then deliver the
//! device callbacks (with `GameInputAsyncEnumeration` semantics) and reading callbacks.
//! `GetCurrentTimestamp` is microseconds on `CLOCK_MONOTONIC`.

use crate::com::{ComObject, Guid, IUnknownVtbl, E_INVALIDARG, E_NOTIMPL, HRESULT, S_OK};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const GAMEINPUT_E_DEVICE_DISCONNECTED: HRESULT = 0x838A_0001u32 as i32;
pub const GAMEINPUT_E_DEVICE_NOT_FOUND: HRESULT = 0x838A_0002u32 as i32;
pub const GAMEINPUT_E_READING_NOT_FOUND: HRESULT = 0x838A_0003u32 as i32;
pub const GAMEINPUT_E_REFERENCE_READING_TOO_OLD: HRESULT = 0x838A_0004u32 as i32;

/// `{11BE2A7E-4254-445A-9C09-FFC40F006918}`
pub const IID_IGAMEINPUT: Guid = Guid::new(
    0x11be2a7e,
    0x4254,
    0x445a,
    [0x9c, 0x09, 0xff, 0xc4, 0x0f, 0x00, 0x69, 0x18],
);
static IIDS: [Guid; 1] = [IID_IGAMEINPUT];

/// Which registration function stored a callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Reading,
    Device,
    GuideButton,
    KeyboardLayout,
}

/// A stored (never fired) callback registration.
#[derive(Clone, Copy, Debug)]
pub struct Registration {
    pub token: u64,
    pub kind: Kind,
    pub context: usize,
    pub callback: usize,
}

#[derive(Default)]
pub struct State {
    next_token: AtomicU64,
    regs: Mutex<Vec<Registration>>,
}

impl State {
    fn register(&self, kind: Kind, context: *mut c_void, callback: *const c_void, out: *mut u64) {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed) + 1;
        self.regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Registration {
                token,
                kind,
                context: context as usize,
                callback: callback as usize,
            });
        if !out.is_null() {
            // SAFETY: non-null; the title passes room for a token.
            unsafe { out.write(token) };
        }
    }
    fn forget(&self, token: u64) -> bool {
        let mut regs = self.regs.lock().unwrap_or_else(|e| e.into_inner());
        let before = regs.len();
        regs.retain(|r| r.token != token);
        regs.len() != before
    }
}

pub type Object = ComObject<Vtbl, State>;
type This = *mut Object;
type Out = *mut *mut c_void;

/// Stored registrations of an object (tests and diagnostics).
///
/// # Safety
/// `this` is a live GameInput object.
pub unsafe fn registrations(this: *mut Object) -> Vec<Registration> {
    // SAFETY: a live GameInput object.
    unsafe {
        (*this)
            .data
            .regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[repr(C)]
pub struct Vtbl {
    unknown: IUnknownVtbl<Vtbl, State>,
    get_current_timestamp: unsafe extern "win64" fn(This) -> u64,
    get_current_reading: unsafe extern "win64" fn(This, u32, *mut c_void, Out) -> HRESULT,
    get_next_reading: unsafe extern "win64" fn(This, *mut c_void, u32, *mut c_void, Out) -> HRESULT,
    get_previous_reading:
        unsafe extern "win64" fn(This, *mut c_void, u32, *mut c_void, Out) -> HRESULT,
    get_temporal_reading: unsafe extern "win64" fn(This, u64, *mut c_void, Out) -> HRESULT,
    // (device, kinds, threshold, context, callback, token out)
    register_reading_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        u32,
        f32,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    // (device, kinds, status filter, async enumeration, context, callback, token out)
    register_device_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        u32,
        u32,
        u32,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    register_guide_button_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    register_keyboard_layout_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    stop_callback: unsafe extern "win64" fn(This, u64),
    unregister_callback: unsafe extern "win64" fn(This, u64, u64) -> u8,
    create_dispatcher: unsafe extern "win64" fn(This, Out) -> HRESULT,
    create_aggregate_device: unsafe extern "win64" fn(This, u32, Out) -> HRESULT,
    find_device_from_id: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_object: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_platform_handle: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_platform_string: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    enable_oem_device_support: unsafe extern "win64" fn(This, u16, u16, u8, u8) -> HRESULT,
    set_focus_policy: unsafe extern "win64" fn(This, u32),
}

fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    hr
}

unsafe extern "win64" fn get_current_timestamp(_: This) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}
unsafe extern "win64" fn get_current_reading(
    _: This,
    _kinds: u32,
    _dev: *mut c_void,
    out: Out,
) -> HRESULT {
    fail(out, GAMEINPUT_E_READING_NOT_FOUND)
}
unsafe extern "win64" fn get_relative_reading(
    _: This,
    reference: *mut c_void,
    _kinds: u32,
    _dev: *mut c_void,
    out: Out,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    fail(out, GAMEINPUT_E_READING_NOT_FOUND)
}
unsafe extern "win64" fn get_temporal_reading(
    _: This,
    _time: u64,
    _dev: *mut c_void,
    out: Out,
) -> HRESULT {
    fail(out, GAMEINPUT_E_READING_NOT_FOUND)
}
unsafe extern "win64" fn register_reading_callback(
    this: This,
    _dev: *mut c_void,
    _kinds: u32,
    _threshold: f32,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this).data.register(Kind::Reading, context, callback, out);
    S_OK
}
unsafe extern "win64" fn register_device_callback(
    this: This,
    _dev: *mut c_void,
    _kinds: u32,
    _status: u32,
    _enumeration: u32,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this).data.register(Kind::Device, context, callback, out);
    S_OK
}
unsafe extern "win64" fn register_guide_button_callback(
    this: This,
    _dev: *mut c_void,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this)
        .data
        .register(Kind::GuideButton, context, callback, out);
    S_OK
}
unsafe extern "win64" fn register_keyboard_layout_callback(
    this: This,
    _dev: *mut c_void,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this)
        .data
        .register(Kind::KeyboardLayout, context, callback, out);
    S_OK
}
unsafe extern "win64" fn stop_callback(this: This, token: u64) {
    (*this).data.forget(token);
}
// (token, timeout in microseconds) -> true when no callback is still running.
unsafe extern "win64" fn unregister_callback(this: This, token: u64, _timeout: u64) -> u8 {
    (*this).data.forget(token);
    1
}
unsafe extern "win64" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    fail(out, E_NOTIMPL)
}
unsafe extern "win64" fn create_aggregate_device(_: This, _kinds: u32, out: Out) -> HRESULT {
    fail(out, E_NOTIMPL)
}
unsafe extern "win64" fn find_device(_: This, value: *const c_void, out: Out) -> HRESULT {
    if value.is_null() {
        return fail(out, E_INVALIDARG);
    }
    fail(out, GAMEINPUT_E_DEVICE_NOT_FOUND)
}
unsafe extern "win64" fn enable_oem_device_support(
    _: This,
    _v: u16,
    _p: u16,
    _i: u8,
    _c: u8,
) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn set_focus_policy(_: This, _policy: u32) {}

static VTBL: Vtbl = Vtbl {
    unknown: IUnknownVtbl::new(),
    get_current_timestamp,
    get_current_reading,
    get_next_reading: get_relative_reading,
    get_previous_reading: get_relative_reading,
    get_temporal_reading,
    register_reading_callback,
    register_device_callback,
    register_guide_button_callback,
    register_keyboard_layout_callback,
    stop_callback,
    unregister_callback,
    create_dispatcher,
    create_aggregate_device,
    find_device_from_id: find_device,
    find_device_from_object: find_device,
    find_device_from_platform_handle: find_device,
    find_device_from_platform_string: find_device,
    enable_oem_device_support,
    set_focus_policy,
};

/// Ordinals of `GameInput.dll`: `(ordinal, name)`.
pub static ORDINALS: &[(u16, &str)] = &[(1, "GameInputCreate")];

win32! {
    module "gameinput";
    // Creates an input object with one reference (no devices).
    Native fn GameInputCreate(out: *mut *mut Object) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        *out = Object::create(&VTBL, &IIDS, State::default());
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{E_NOINTERFACE, E_POINTER, IID_IUNKNOWN};

    fn create() -> (*mut Object, &'static Vtbl) {
        let mut o: *mut Object = std::ptr::null_mut();
        assert_eq!(unsafe { GameInputCreate(&mut o) }, S_OK);
        assert!(!o.is_null());
        (o, unsafe { &*(*(o as *const *const Vtbl)) })
    }

    #[test]
    fn create_null_out_and_ordinal() {
        assert_eq!(
            unsafe { GameInputCreate(std::ptr::null_mut()) },
            E_INVALIDARG
        );
        let e = crate::lookup_ordinal("GameInput.dll", 1).unwrap();
        assert_eq!(e.name, "GameInputCreate");
        assert!(crate::lookup("GameInput.dll", "GameInputCreate").is_some());
        assert!(crate::lookup_ordinal("GameInput.dll", 2).is_none());
    }

    #[test]
    fn query_and_release() {
        unsafe {
            let (o, v) = create();
            let mut out = std::ptr::null_mut();
            for iid in [IID_IGAMEINPUT, IID_IUNKNOWN] {
                assert_eq!((v.unknown.query_interface)(o, &iid, &mut out), S_OK);
                assert_eq!(out, o);
            }
            assert_eq!((*o).ref_count(), 3);
            let bad = Guid::new(1, 1, 1, [1; 8]);
            assert_eq!(
                (v.unknown.query_interface)(o, &bad, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                (v.unknown.query_interface)(o, &bad, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!((v.unknown.release)(o), 2);
            assert_eq!((v.unknown.release)(o), 1);
            assert_eq!((v.unknown.add_ref)(o), 2);
            assert_eq!((v.unknown.release)(o), 1);
            assert_eq!((v.unknown.release)(o), 0);
        }
    }

    #[test]
    fn readings_not_found() {
        unsafe {
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let mut out: *mut c_void = 8 as *mut c_void;
            assert_eq!(
                (v.get_current_reading)(o, 1, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert!(out.is_null());
            let r = 16 as *mut c_void;
            assert_eq!(
                (v.get_next_reading)(o, r, 1, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert_eq!(
                (v.get_previous_reading)(o, r, 1, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert_eq!((v.get_next_reading)(o, np, 1, np, &mut out), E_INVALIDARG);
            assert_eq!(
                (v.get_temporal_reading)(o, 5, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            // NULL out pointers are tolerated.
            assert_eq!(
                (v.get_current_reading)(o, 1, np, std::ptr::null_mut()),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert_eq!((v.create_dispatcher)(o, &mut out), E_NOTIMPL);
            assert_eq!((v.create_aggregate_device)(o, 1, &mut out), E_NOTIMPL);
            assert_eq!(
                (v.find_device_from_id)(o, r, &mut out),
                GAMEINPUT_E_DEVICE_NOT_FOUND
            );
            assert_eq!((v.find_device_from_id)(o, np, &mut out), E_INVALIDARG);
            assert_eq!((v.enable_oem_device_support)(o, 1, 2, 3, 4), S_OK);
            (v.set_focus_policy)(o, 1);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn timestamp_is_monotonic() {
        unsafe {
            let (o, v) = create();
            let a = (v.get_current_timestamp)(o);
            std::thread::sleep(std::time::Duration::from_millis(2));
            let b = (v.get_current_timestamp)(o);
            assert!(b >= a + 1500, "{a} {b}");
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn callbacks_get_tokens() {
        unsafe {
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let (mut t1, mut t2, mut t3) = (0u64, 0u64, 0u64);
            let cb = 0x1234 as *const c_void;
            assert_eq!(
                (v.register_device_callback)(o, np, 1, 3, 0, np, cb, &mut t1),
                S_OK
            );
            assert_eq!(
                (v.register_reading_callback)(o, np, 1, 0.0, np, cb, &mut t2),
                S_OK
            );
            assert_eq!(
                (v.register_guide_button_callback)(o, np, np, cb, &mut t3),
                S_OK
            );
            assert_eq!(
                (v.register_keyboard_layout_callback)(o, np, np, cb, std::ptr::null_mut()),
                S_OK
            );
            assert!(t1 != 0 && t2 != t1 && t3 != t2);
            assert_eq!(registrations(o).len(), 4);
            assert_eq!(registrations(o)[0].kind, Kind::Device);
            (v.stop_callback)(o, t1);
            assert_eq!((v.unregister_callback)(o, t2, 1000), 1);
            assert_eq!((v.unregister_callback)(o, 999, 0), 1);
            assert_eq!(registrations(o).len(), 2);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn concurrent_refcount() {
        unsafe {
            let (o, v) = create();
            let p = o as usize;
            let hs: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || {
                        let o = p as *mut Object;
                        let v = &**(o as *const *const Vtbl);
                        for _ in 0..1000 {
                            (v.unknown.add_ref)(o);
                            (v.unknown.release)(o);
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            assert_eq!((*o).ref_count(), 1);
            assert_eq!((v.unknown.release)(o), 0);
        }
    }
}
