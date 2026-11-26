//! The user provider of the game runtime (`XUser*`): one offline local user, signed in, never a
//! guest, with every privilege, served from `QueryApiImpl`. Adding the user completes at once
//! through [`crate::xasync`] like any other async call, so the title's completion callback and
//! `XUserAddResult` work. No input device is associated with the user (`find by device` answers
//! "no such user"), tokens are never available (offline), and the change callbacks are stored
//! with a token and never fire because the user never changes.
//!
//! Slot order, interface ids and the meaning of each method come from the public GDK headers as
//! read through another loader's tables; no code is shared. The gamertag is [`GAMERTAG`].
//! Methods the title calls that are not implemented are [`trap`]s: logged once with their slot,
//! answered `E_NOTIMPL`. No Wine oracle exists.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::com::Guid;
use crate::xasync::{self, AsyncBlock, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER};
use crate::xgameruntime::{new_object, slot_trap, table, Table, AREA_USER, S_OK};

pub type HRESULT = i32;

/// The user provider's API id (also an interface id titles ask with).
pub const PROVIDER: Guid = Guid::new(
    0x01ac_d177,
    0x91f9,
    0x4763,
    [0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0],
);
/// Its main interface.
pub const IID: Guid = Guid::new(
    0xeb9b_f948,
    0x18dc,
    0x4d82,
    [0xbb, 0xcc, 0x40, 0xe0, 0xa8, 0x09, 0xc4, 0xc0],
);
/// The interface a title asks for to read a gamertag.
pub const GAMERTAG_IID: Guid = Guid::new(
    0xcef4_fac0,
    0x7676,
    0x4a94,
    [0xa1, 0x19, 0x4c, 0x43, 0xf9, 0xeb, 0x5b, 0x74],
);

/// The documented "no such user" error.
pub const USER_NOT_FOUND: HRESULT = 0x8924_5104u32 as i32;
/// The documented "failed to get token": tokens come from the service.
pub const FAILED_TO_GET_TOKEN: HRESULT = 0x8924_5111u32 as i32;

pub const XUID: u64 = 0x0009_0000_0000_0001;
pub const LOCAL_ID: u64 = 1;
pub const GAMERTAG: &str = "Player";

/// The only user: a handle is the address of this, duplicating and closing are free.
static USER: u8 = 0;
type Handle = *const u8;

fn user() -> Handle {
    &USER
}

fn put<T>(out: *mut T, value: T) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a `T`.
    unsafe { out.write(value) };
    S_OK
}

fn found(is_it: bool, out: *mut Handle) -> HRESULT {
    if is_it {
        put(out, user())
    } else {
        USER_NOT_FOUND
    }
}

type This = *mut c_void;

extern "win64" fn duplicate(_: This, _: Handle, out: *mut Handle) -> HRESULT {
    put(out, user())
}
extern "win64" fn close(_: This, _: Handle) {}
extern "win64" fn compare(_: This, a: Handle, b: Handle) -> i32 {
    i32::from(a != b)
}

/// Identity of our add calls.
static ADD: u8 = 0;

/// Any options add the one user, silently: the call is over at once, its result is the handle.
extern "win64" fn add_async(_: This, options: u32, block: *mut AsyncBlock) -> HRESULT {
    eprintln!("nativ64: XUserAddAsync (options {options:#x}): the offline local user");
    let handle = (user() as usize).to_le_bytes().to_vec();
    xasync::finished(block, (&raw const ADD).cast(), S_OK, handle)
}
extern "win64" fn add_result(_: This, block: *mut AsyncBlock, out: *mut Handle) -> HRESULT {
    xasync::XAsyncGetResult(
        block,
        (&raw const ADD).cast(),
        size_of::<Handle>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}
extern "win64" fn get_local_id(_: This, _: Handle, out: *mut u64) -> HRESULT {
    put(out, LOCAL_ID)
}
extern "win64" fn find_by_local_id(_: This, id: u64, out: *mut Handle) -> HRESULT {
    found(id == LOCAL_ID, out)
}
extern "win64" fn get_id(_: This, _: Handle, out: *mut u64) -> HRESULT {
    put(out, XUID)
}
extern "win64" fn find_by_id(_: This, id: u64, out: *mut Handle) -> HRESULT {
    found(id == XUID, out)
}
extern "win64" fn get_is_guest(_: This, _: Handle, out: *mut bool) -> HRESULT {
    put(out, false)
}
/// 0: signed in.
extern "win64" fn get_state(_: This, _: Handle, out: *mut u32) -> HRESULT {
    put(out, 0)
}

/// Copies `text` and a NUL into the title's `size`-byte buffer, reporting the bytes needed.
fn put_string(text: &str, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let len = text.len() + 1;
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(len) };
    }
    if buffer.is_null() || size < len {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size >= len` bytes.
    unsafe {
        buffer.copy_from_nonoverlapping(text.as_ptr(), text.len());
        buffer.add(text.len()).write(0);
    }
    S_OK
}

/// The gamertag as the component asked for (classic, modern, modern suffix, unique modern): it
/// has no suffix, so the suffix is empty and every other component is the name itself.
extern "win64" fn get_gamertag(
    _: This,
    _: Handle,
    component: u32,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    const MODERN_SUFFIX: u32 = 2;
    let text = if component == MODERN_SUFFIX {
        ""
    } else {
        GAMERTAG
    };
    put_string(text, size, buffer, used)
}
/// Every privilege is granted; deny reason 0 (none).
extern "win64" fn check_privilege(
    _: This,
    _: Handle,
    _options: u32,
    _privilege: u32,
    has: *mut bool,
    reason: *mut u32,
) -> HRESULT {
    if !reason.is_null() {
        // SAFETY: checked non-null.
        unsafe { reason.write(0) };
    }
    put(has, true)
}
/// 3: adult.
extern "win64" fn get_age_group(_: This, _: Handle, out: *mut u32) -> HRESULT {
    put(out, 3)
}

static RESOLVE: u8 = 0;
/// Nothing to resolve: every privilege is granted.
extern "win64" fn resolve_privilege_async(
    _: This,
    _: Handle,
    _options: u32,
    _privilege: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    xasync::finished(block, (&raw const RESOLVE).cast(), S_OK, Vec::new())
}
extern "win64" fn resolve_privilege_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const RESOLVE).cast(), 0, none, none.cast())
}

static TOKEN: u8 = 0;
/// Tokens and signatures come from the service: offline, the call fails.
#[allow(clippy::too_many_arguments)]
extern "win64" fn token_async(
    _: This,
    _: Handle,
    _options: u32,
    _method: *const c_char,
    url: *const c_char,
    _header_count: usize,
    _headers: *const c_void,
    _body_size: usize,
    _body: *const c_void,
    block: *mut AsyncBlock,
) -> HRESULT {
    let url = if url.is_null() {
        String::new()
    } else {
        // SAFETY: a NUL-terminated string of the title's.
        unsafe { std::ffi::CStr::from_ptr(url) }
            .to_string_lossy()
            .into_owned()
    };
    eprintln!("nativ64: XUserGetTokenAndSignatureAsync {url}: offline, refused");
    xasync::finished(
        block,
        (&raw const TOKEN).cast(),
        FAILED_TO_GET_TOKEN,
        Vec::new(),
    )
}
extern "win64" fn token_size(_: This, block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    xasync::XAsyncGetResultSize(block, out)
}
extern "win64" fn token_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    buffer: *mut c_void,
    _data: *mut *const c_void,
    used: *mut usize,
) -> HRESULT {
    xasync::XAsyncGetResult(block, (&raw const TOKEN).cast(), size, buffer, used)
}

/// Change callbacks: the user never changes, so they are never called.
extern "win64" fn register(
    _: This,
    _queue: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    put(out, NEXT.fetch_add(1, Ordering::Relaxed))
}
extern "win64" fn unregister(_: This, _token: u64, _wait: bool) -> bool {
    true
}

const fn t<const S: usize>() -> extern "win64" fn() -> i32 {
    slot_trap::<AREA_USER, S>
}

const SLOTS: usize = 35;
static VTBL: Table<SLOTS> = table!(SLOTS;
    duplicate,                // 3
    close,                    // 4
    compare,                  // 5
    t::<6>(),                 // maximum users
    add_async,                // 7
    add_result,               // 8
    get_local_id,             // 9
    find_by_local_id,         // 10
    get_id,                   // 11
    find_by_id,               // 12
    get_is_guest,             // 13
    get_state,                // 14
    get_gamertag,             // 15 older gamertag slot
    t::<16>(),                // gamer picture: begin
    t::<17>(),                // result size
    t::<18>(),                // result
    get_age_group,            // 19
    check_privilege,          // 20
    resolve_privilege_async,  // 21 with UI
    resolve_privilege_result, // 22
    token_async,              // 23 token and signature
    token_size,               // 24
    token_result,             // 25
    t::<26>(),                // the same, UTF-16: begin
    t::<27>(),                // result size
    t::<28>(),                // result
    t::<29>(),                // resolve issue with UI: begin
    t::<30>(),                // result
    t::<31>(),                // the same, UTF-16: begin
    t::<32>(),                // result
    register,                 // 33 change event
    unregister,               // 34
);
static IIDS: [Guid; 2] = [IID, PROVIDER];

const GAMERTAG_SLOTS: usize = 4;
static GAMERTAG_VTBL: Table<GAMERTAG_SLOTS> = table!(GAMERTAG_SLOTS; get_gamertag);
static GAMERTAG_IIDS: [Guid; 1] = [GAMERTAG_IID];

/// The provider's objects for `QueryApiImpl`. A title also asks with the provider's id as the
/// interface; its calls are the same slots.
pub fn query(api: &Guid, iid: &Guid) -> Option<*mut c_void> {
    if *api != PROVIDER {
        return None;
    }
    if *iid == IID || *iid == PROVIDER {
        Some(new_object(&VTBL, &IIDS))
    } else if *iid == GAMERTAG_IID {
        Some(new_object(&GAMERTAG_VTBL, &GAMERTAG_IIDS))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{release, ComObject};
    use crate::xgameruntime::method;
    use std::sync::atomic::AtomicU32;

    fn object(iid: &Guid) -> *mut c_void {
        query(&PROVIDER, iid).unwrap()
    }

    fn block(queue: *const xasync::Queue) -> AsyncBlock {
        AsyncBlock {
            queue,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    /// The user handle after a completed add.
    unsafe fn added(o: *mut c_void) -> Handle {
        let mut b = block(std::ptr::null());
        let add: extern "win64" fn(This, u32, *mut AsyncBlock) -> HRESULT = method(o, 7);
        assert_eq!(add(o, 1, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let result: extern "win64" fn(This, *mut AsyncBlock, *mut Handle) -> HRESULT = method(o, 8);
        let mut h = std::ptr::null();
        assert_eq!(result(o, &mut b, &mut h), S_OK);
        h
    }

    #[test]
    fn only_the_user_provider_answers() {
        assert!(query(&PROVIDER, &IID).is_some());
        assert!(query(&PROVIDER, &PROVIDER).is_some());
        assert!(query(&PROVIDER, &GAMERTAG_IID).is_some());
        assert!(query(&PROVIDER, &xasync::PROVIDER).is_none());
        assert!(query(&xasync::PROVIDER, &IID).is_none());
    }

    #[test]
    fn add_completes_through_the_queue_and_yields_the_handle() {
        static CALLED: AtomicU32 = AtomicU32::new(0);
        extern "win64" fn done(_: *mut AsyncBlock) {
            CALLED.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            let o = object(&IID);
            let mut queue = std::ptr::null();
            assert_eq!(xasync::XTaskQueueCreate(0, 0, &mut queue), S_OK);
            let mut b = block(queue);
            b.callback = Some(done);
            let add: extern "win64" fn(This, u32, *mut AsyncBlock) -> HRESULT = method(o, 7);
            assert_eq!(add(o, 0, &mut b), S_OK);
            // Complete, but the callback waits for the title to dispatch the completion port.
            assert_eq!(xasync::XAsyncGetStatus(&mut b, false), S_OK);
            assert_eq!(CALLED.load(Ordering::SeqCst), 0);
            assert!(!xasync::XTaskQueueDispatch(queue, 0, 0));
            assert!(xasync::XTaskQueueDispatch(queue, 1, 0));
            assert_eq!(CALLED.load(Ordering::SeqCst), 1);
            let mut size = 0;
            assert_eq!(xasync::XAsyncGetResultSize(&mut b, &mut size), S_OK);
            assert_eq!(size, size_of::<Handle>());
            let result: extern "win64" fn(This, *mut AsyncBlock, *mut Handle) -> HRESULT =
                method(o, 8);
            let mut h = std::ptr::null();
            assert_eq!(result(o, &mut b, &mut h), S_OK);
            assert_eq!(h, user());
            // The result was taken: the call is over.
            assert_eq!(result(o, &mut b, &mut h), xasync::E_ILLEGAL_METHOD_CALL);
            xasync::XTaskQueueCloseHandle(queue);
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn identity_methods() {
        unsafe {
            let o = object(&IID);
            let h = added(o);
            let none: Handle = std::ptr::null();
            let dup: extern "win64" fn(This, Handle, *mut Handle) -> HRESULT = method(o, 3);
            let mut d = none;
            assert_eq!(dup(o, h, &mut d), S_OK);
            assert_eq!(d, h);
            assert_eq!(dup(o, h, std::ptr::null_mut()), E_INVALIDARG);
            let close: extern "win64" fn(This, Handle) = method(o, 4);
            close(o, h);
            let compare: extern "win64" fn(This, Handle, Handle) -> i32 = method(o, 5);
            assert_eq!(compare(o, h, d), 0);
            assert_ne!(compare(o, h, none), 0);

            let get_u64 =
                |slot| -> extern "win64" fn(This, Handle, *mut u64) -> HRESULT { method(o, slot) };
            let mut v = 0;
            assert_eq!(get_u64(9)(o, h, &mut v), S_OK);
            assert_eq!(v, LOCAL_ID);
            assert_eq!(get_u64(11)(o, h, &mut v), S_OK);
            assert_eq!(v, XUID);
            let find =
                |slot| -> extern "win64" fn(This, u64, *mut Handle) -> HRESULT { method(o, slot) };
            assert_eq!(find(10)(o, LOCAL_ID, &mut d), S_OK);
            assert_eq!(d, h);
            assert_eq!(find(10)(o, 7, &mut d), USER_NOT_FOUND);
            assert_eq!(find(12)(o, XUID, &mut d), S_OK);
            assert_eq!(find(12)(o, 7, &mut d), USER_NOT_FOUND);

            let guest: extern "win64" fn(This, Handle, *mut bool) -> HRESULT = method(o, 13);
            let mut g = true;
            assert_eq!(guest(o, h, &mut g), S_OK);
            assert!(!g);
            let state: extern "win64" fn(This, Handle, *mut u32) -> HRESULT = method(o, 14);
            let mut s = 9;
            assert_eq!(state(o, h, &mut s), S_OK);
            assert_eq!(s, 0);
            let age: extern "win64" fn(This, Handle, *mut u32) -> HRESULT = method(o, 19);
            assert_eq!(age(o, h, &mut s), S_OK);
            assert_eq!(s, 3);
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn gamertag_through_both_objects() {
        type Get = extern "win64" fn(This, Handle, u32, usize, *mut u8, *mut usize) -> HRESULT;
        unsafe {
            let o = object(&IID);
            let g = object(&GAMERTAG_IID);
            let h = added(o);
            let expected: [&[u8]; 4] = [b"Player\0", b"Player\0", b"\0", b"Player\0"];
            for (obj, slot) in [(o, 15), (g, 3)] {
                let get: Get = method(obj, slot);
                for (component, expected) in (0..).zip(expected) {
                    let (mut buf, mut used) = ([0xffu8; 16], 0);
                    assert_eq!(
                        get(obj, h, component, 16, buf.as_mut_ptr(), &mut used),
                        S_OK
                    );
                    assert_eq!(&buf[..used], expected, "component {component}");
                }
                // Too small: the needed size is reported, nothing is written.
                let (mut buf, mut used) = ([0xffu8; 2], 0);
                assert_eq!(
                    get(obj, h, 0, 2, buf.as_mut_ptr(), &mut used),
                    E_NOT_SUFFICIENT_BUFFER
                );
                assert_eq!((used, buf), (7, [0xff; 2]));
            }
            release(o as *mut ComObject<Table<SLOTS>, ()>);
            release(g as *mut ComObject<Table<GAMERTAG_SLOTS>, ()>);
        }
    }

    #[test]
    fn privileges_are_granted_and_resolving_is_a_no_op() {
        unsafe {
            let o = object(&IID);
            let h = added(o);
            let check: extern "win64" fn(This, Handle, u32, u32, *mut bool, *mut u32) -> HRESULT =
                method(o, 20);
            let (mut has, mut why) = (false, 5);
            assert_eq!(check(o, h, 0, 211, &mut has, &mut why), S_OK);
            assert!(has);
            assert_eq!(why, 0);
            assert_eq!(check(o, h, 0, 211, &mut has, std::ptr::null_mut()), S_OK);
            assert_eq!(
                check(o, h, 0, 211, std::ptr::null_mut(), &mut why),
                E_INVALIDARG
            );

            let begin: extern "win64" fn(This, Handle, u32, u32, *mut AsyncBlock) -> HRESULT =
                method(o, 21);
            let end: extern "win64" fn(This, *mut AsyncBlock) -> HRESULT = method(o, 22);
            let mut b = block(std::ptr::null());
            assert_eq!(begin(o, h, 0, 211, &mut b), S_OK);
            assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
            assert_eq!(end(o, &mut b), S_OK);
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn tokens_are_refused_offline() {
        unsafe {
            let o = object(&IID);
            let h = added(o);
            let begin: extern "win64" fn(
                This,
                Handle,
                u32,
                *const c_char,
                *const c_char,
                usize,
                *const c_void,
                usize,
                *const c_void,
                *mut AsyncBlock,
            ) -> HRESULT = method(o, 23);
            let size: extern "win64" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT =
                method(o, 24);
            let result: extern "win64" fn(
                This,
                *mut AsyncBlock,
                usize,
                *mut c_void,
                *mut *const c_void,
                *mut usize,
            ) -> HRESULT = method(o, 25);
            let np = std::ptr::null();
            let mut b = block(std::ptr::null());
            let url = c"https://example.invalid/".as_ptr();
            assert_eq!(
                begin(o, h, 0, c"GET".as_ptr(), url, 0, np, 0, np, &mut b),
                S_OK
            );
            assert_eq!(xasync::XAsyncGetStatus(&mut b, true), FAILED_TO_GET_TOKEN);
            let mut n = 0;
            assert_eq!(size(o, &mut b, &mut n), FAILED_TO_GET_TOKEN);
            let mut used = 0;
            assert_eq!(
                result(
                    o,
                    &mut b,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut used
                ),
                FAILED_TO_GET_TOKEN
            );
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn change_callbacks_get_distinct_tokens() {
        unsafe {
            let o = object(&IID);
            let reg: extern "win64" fn(
                This,
                *const c_void,
                *mut c_void,
                *const c_void,
                *mut u64,
            ) -> HRESULT = method(o, 33);
            let unreg: extern "win64" fn(This, u64, bool) -> bool = method(o, 34);
            let np = std::ptr::null_mut();
            let (mut a, mut b) = (0, 0);
            assert_eq!(reg(o, np, np, np, &mut a), S_OK);
            assert_eq!(reg(o, np, np, np, &mut b), S_OK);
            assert!(a != 0 && a != b);
            assert_eq!(reg(o, np, np, np, std::ptr::null_mut()), E_INVALIDARG);
            assert!(unreg(o, a, true));
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn unimplemented_slots_answer_not_implemented() {
        unsafe {
            let o = object(&IID);
            let maximum: extern "win64" fn(This) -> HRESULT = method(o, 6);
            assert_eq!(maximum(o), crate::xgameruntime::E_NOTIMPL);
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }
}
