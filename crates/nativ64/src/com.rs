//! A small COM toolkit for host objects handed to the title (GameInput, the game runtime, D3D12).
//!
//! A COM object is a pointer to a pointer to a vtable of `extern "win64"` functions, the first
//! three being `IUnknown`. [`ComObject<V, T>`] is that layout: a vtable pointer first, then a
//! reference count, the IIDs the object answers to and the Rust payload `T`. It is created with
//! [`ComObject::create`] (count 1, leaked as a raw pointer) and freed by `Release` at zero.
//!
//! Declaring a vtable: a `#[repr(C)]` struct whose first field is [`IUnknownVtbl`], followed by
//! one `unsafe extern "win64" fn(this: *mut ComObject<V, T>, ...)` field per method in the
//! interface's published order; a `static` instance fills the first field with
//! [`IUnknownVtbl::new`]. See the tests below for a complete example.
//!
//! Conventions: out pointers are cleared before an error is returned; a NULL `this` is not
//! checked (the title is trusted to pass a live object), but NULL out and IID pointers are.

use std::sync::atomic::{fence, AtomicU32, Ordering};

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
pub const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;
pub const E_POINTER: HRESULT = 0x8000_4003u32 as i32;
pub const E_FAIL: HRESULT = 0x8000_4005u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;

/// A GUID in its Windows layout (16 bytes, little-endian fields).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// `{00000000-0000-0000-C000-000000000046}`
pub const IID_IUNKNOWN: Guid = Guid::new(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

/// The first three vtable slots of every COM interface.
#[repr(C)]
pub struct IUnknownVtbl<V: 'static, T: 'static> {
    pub query_interface: unsafe extern "win64" fn(
        *mut ComObject<V, T>,
        *const Guid,
        *mut *mut ComObject<V, T>,
    ) -> HRESULT,
    pub add_ref: unsafe extern "win64" fn(*mut ComObject<V, T>) -> u32,
    pub release: unsafe extern "win64" fn(*mut ComObject<V, T>) -> u32,
}

impl<V: 'static, T: 'static> IUnknownVtbl<V, T> {
    pub const fn new() -> Self {
        IUnknownVtbl {
            query_interface: query_interface::<V, T>,
            add_ref: add_ref::<V, T>,
            release: release::<V, T>,
        }
    }
}

impl<V: 'static, T: 'static> Default for IUnknownVtbl<V, T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A COM object: vtable pointer first (what the title sees), then our state.
#[repr(C)]
pub struct ComObject<V: 'static, T: 'static> {
    vtbl: &'static V,
    refs: AtomicU32,
    iids: &'static [Guid],
    pub data: T,
}

impl<V: 'static, T: 'static> ComObject<V, T> {
    /// A new object with one reference. It answers `QueryInterface` for `IUnknown` and `iids`.
    /// `vtbl`'s first field must be an [`IUnknownVtbl<V, T>`].
    pub fn create(vtbl: &'static V, iids: &'static [Guid], data: T) -> *mut Self {
        Box::into_raw(Box::new(ComObject {
            vtbl,
            refs: AtomicU32::new(1),
            iids,
            data,
        }))
    }

    /// The current reference count (diagnostics and tests).
    pub fn ref_count(&self) -> u32 {
        self.refs.load(Ordering::Acquire)
    }
}

/// `IUnknown::QueryInterface`: `IUnknown` and the object's IIDs return the same pointer.
///
/// # Safety
/// `this` is a live object; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn query_interface<V: 'static, T: 'static>(
    this: *mut ComObject<V, T>,
    iid: *const Guid,
    out: *mut *mut ComObject<V, T>,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if iid.is_null() {
        return E_POINTER;
    }
    let iid = *iid;
    if iid == IID_IUNKNOWN || (*this).iids.contains(&iid) {
        add_ref::<V, T>(this);
        *out = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// # Safety
/// `this` is a live object.
pub unsafe extern "win64" fn add_ref<V: 'static, T: 'static>(this: *mut ComObject<V, T>) -> u32 {
    (*this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

/// Drops one reference and frees the object at zero.
///
/// # Safety
/// `this` is a live object created by [`ComObject::create`]; the caller owns a reference.
pub unsafe extern "win64" fn release<V: 'static, T: 'static>(this: *mut ComObject<V, T>) -> u32 {
    let left = (*this).refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        fence(Ordering::Acquire);
        drop(Box::from_raw(this));
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    type Obj = ComObject<Vtbl, u32>;
    #[repr(C)]
    struct Vtbl {
        unknown: IUnknownVtbl<Vtbl, u32>,
        get: unsafe extern "win64" fn(*mut Obj) -> u32,
    }
    unsafe extern "win64" fn get(this: *mut Obj) -> u32 {
        (*this).data
    }
    static VTBL: Vtbl = Vtbl {
        unknown: IUnknownVtbl::new(),
        get,
    };
    const IID_A: Guid = Guid::new(1, 2, 3, [4; 8]);
    static IIDS: [Guid; 1] = [IID_A];

    #[test]
    fn query_ref_release() {
        unsafe {
            let o = Obj::create(&VTBL, &IIDS, 42);
            let vt = &*(o as *const &Vtbl);
            assert_eq!((vt.get)(o), 42);
            let mut out = std::ptr::null_mut();
            assert_eq!((vt.unknown.query_interface)(o, &IID_A, &mut out), S_OK);
            assert_eq!(out, o);
            assert_eq!((*o).ref_count(), 2);
            assert_eq!(
                (vt.unknown.query_interface)(o, &IID_IUNKNOWN, &mut out),
                S_OK
            );
            assert_eq!((vt.unknown.release)(o), 2);
            let other = Guid::new(9, 9, 9, [9; 8]);
            out = std::ptr::dangling_mut::<Obj>();
            assert_eq!(
                (vt.unknown.query_interface)(o, &other, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                (vt.unknown.query_interface)(o, &IID_A, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                (vt.unknown.query_interface)(o, std::ptr::null(), &mut out),
                E_POINTER
            );
            assert_eq!((vt.unknown.add_ref)(o), 3);
            assert_eq!((vt.unknown.release)(o), 2);
            assert_eq!((vt.unknown.release)(o), 1);
            assert_eq!((vt.unknown.release)(o), 0);
        }
    }
}
