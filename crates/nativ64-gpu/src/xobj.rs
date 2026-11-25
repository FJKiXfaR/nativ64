//! Xbox-flavoured COM objects: the layout the title's inlined code expects, and the trap loop.
//!
//! `nativ64::com::ComObject` keeps the vtable pointer first and its own fields after it, which is
//! fine for objects only we look at. The console's D3D12 objects are different: the title's
//! inlined code reads fields at fixed offsets from the object pointer (`+8` and up), so the
//! layout after the vtable pointer is the console's. Our bookkeeping (refcount, IID list,
//! interface name) therefore lives in a prefix placed *before* the vtable pointer in the same
//! allocation, where no console offset can collide with it. The pointer given to the title points
//! at the vtable pointer; the console fields (a `#[repr(C)]` `T` chosen by each object) follow.
//!
//! Every slot of every table starts as a trap that names its interface and slot, prints it and
//! aborts. The loop is: run, implement the slot that stopped, rerun.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::atomic::{fence, AtomicU32, Ordering};

pub use nativ64::com::{Guid, E_NOINTERFACE, E_POINTER, HRESULT, IID_IUNKNOWN, S_OK};

/// The interfaces whose tables carry traps, named for the trap message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Iface {
    Device,
    CommandQueue,
    CommandList,
    CommandAllocator,
    Resource,
    Heap,
    DescriptorHeap,
    Fence,
    PipelineState,
    RootSignature,
    QueryHeap,
    CommandSignature,
}

const IFACES: [Iface; 12] = [
    Iface::Device,
    Iface::CommandQueue,
    Iface::CommandList,
    Iface::CommandAllocator,
    Iface::Resource,
    Iface::Heap,
    Iface::DescriptorHeap,
    Iface::Fence,
    Iface::PipelineState,
    Iface::RootSignature,
    Iface::QueryHeap,
    Iface::CommandSignature,
];

impl Iface {
    pub const fn name(self) -> &'static str {
        match self {
            Iface::Device => "ID3D12Device",
            Iface::CommandQueue => "ID3D12CommandQueue",
            Iface::CommandList => "ID3D12GraphicsCommandList",
            Iface::CommandAllocator => "ID3D12CommandAllocator",
            Iface::Resource => "ID3D12Resource",
            Iface::Heap => "ID3D12Heap",
            Iface::DescriptorHeap => "ID3D12DescriptorHeap",
            Iface::Fence => "ID3D12Fence",
            Iface::PipelineState => "ID3D12PipelineState",
            Iface::RootSignature => "ID3D12RootSignature",
            Iface::QueryHeap => "ID3D12QueryHeap",
            Iface::CommandSignature => "ID3D12CommandSignature",
        }
    }
}

/// The text a trap prints, e.g. `nativ64: unimplemented ID3D12Device slot 12 (+0x60)`.
pub fn trap_message(iface: Iface, slot: usize) -> String {
    format!(
        "nativ64: unimplemented {} slot {} (+{:#x})",
        iface.name(),
        slot,
        slot * 8
    )
}

/// The body of every trap: say which slot was called, then stop.
extern "win64" fn trap<const IFACE: usize, const SLOT: usize>() -> ! {
    eprintln!("{}", trap_message(IFACES[IFACE], SLOT));
    // SAFETY: abort never returns.
    unsafe { libc::abort() }
}

/// Most slots a table can have (the largest D3D12 interface has well under that).
pub const MAX_SLOTS: usize = 256;

type Row = [*const (); 16];

macro_rules! row {
    ($i:literal, $b:literal) => {
        [
            trap::<$i, { $b }> as *const (),
            trap::<$i, { $b + 1 }> as *const (),
            trap::<$i, { $b + 2 }> as *const (),
            trap::<$i, { $b + 3 }> as *const (),
            trap::<$i, { $b + 4 }> as *const (),
            trap::<$i, { $b + 5 }> as *const (),
            trap::<$i, { $b + 6 }> as *const (),
            trap::<$i, { $b + 7 }> as *const (),
            trap::<$i, { $b + 8 }> as *const (),
            trap::<$i, { $b + 9 }> as *const (),
            trap::<$i, { $b + 10 }> as *const (),
            trap::<$i, { $b + 11 }> as *const (),
            trap::<$i, { $b + 12 }> as *const (),
            trap::<$i, { $b + 13 }> as *const (),
            trap::<$i, { $b + 14 }> as *const (),
            trap::<$i, { $b + 15 }> as *const (),
        ]
    };
}

macro_rules! iface_traps {
    ($i:literal) => {
        [
            row!($i, 0),
            row!($i, 16),
            row!($i, 32),
            row!($i, 48),
            row!($i, 64),
            row!($i, 80),
            row!($i, 96),
            row!($i, 112),
            row!($i, 128),
            row!($i, 144),
            row!($i, 160),
            row!($i, 176),
            row!($i, 192),
            row!($i, 208),
            row!($i, 224),
            row!($i, 240),
        ]
    };
}

macro_rules! all_traps {
    ($($i:literal)*) => { [$(iface_traps!($i)),*] };
}

/// `TRAPS[iface][slot / 16][slot % 16]`: one distinct trap per (interface, slot).
#[allow(clippy::large_const_arrays)] // only read in const fns, never copied at run time
const TRAPS: [[Row; 16]; 12] = all_traps!(0 1 2 3 4 5 6 7 8 9 10 11);

/// A vtable: `N` function pointers, every one a trap until overridden.
#[repr(C)]
pub struct SlotTable<const N: usize> {
    slots: [*const (); N],
}

// SAFETY: the table is immutable once built and holds only code addresses.
unsafe impl<const N: usize> Sync for SlotTable<N> {}

impl<const N: usize> SlotTable<N> {
    /// A table whose every slot traps with `iface` and its own index.
    pub const fn new(iface: Iface) -> Self {
        assert!(N <= MAX_SLOTS, "SlotTable: at most 256 slots");
        let mut slots = [std::ptr::null::<()>(); N];
        let mut i = 0;
        while i < N {
            slots[i] = TRAPS[iface as usize][i / 16][i % 16];
            i += 1;
        }
        SlotTable { slots }
    }

    /// Replaces slot `slot` with `f`. Panics (at compile time in a const) when out of range.
    pub const fn with(mut self, slot: usize, f: *const ()) -> Self {
        assert!(slot < N, "SlotTable::with: slot out of range");
        self.slots[slot] = f;
        self
    }

    /// Fills slots 0-2 with the generic `IUnknown` functions for objects holding a `T`.
    pub const fn with_iunknown<T>(self) -> Self {
        self.with(0, query_interface::<T> as *const ())
            .with(1, add_ref::<T> as *const ())
            .with(2, release::<T> as *const ())
    }

    /// The function in slot `slot`.
    pub const fn slot(&self, slot: usize) -> *const () {
        self.slots[slot]
    }
}

/// Our bookkeeping, placed right before the vtable pointer.
#[repr(C)]
pub struct Prefix {
    refs: AtomicU32,
    iface: Iface,
    iids: &'static [Guid],
}

impl Prefix {
    pub fn ref_count(&self) -> u32 {
        self.refs.load(Ordering::Acquire)
    }

    pub fn iface(&self) -> Iface {
        self.iface
    }
}

/// The whole allocation; the title's pointer is the address of `vtbl`.
#[repr(C)]
struct Block<T> {
    prefix: Prefix,
    vtbl: *const (),
    data: T,
}

/// Offset from the allocation start to the pointer the title holds.
const PREFIX_SIZE: usize = size_of::<Prefix>();

/// Namespace for the object functions; `T` is the console-layout part at `+8`.
pub struct XObject<T>(PhantomData<T>);

impl<T> XObject<T> {
    /// A new object with one reference. The result is what the title holds: it points at the
    /// vtable pointer (`table`), with `data` at `+8`. `QueryInterface` answers `IUnknown` and `iids`.
    pub fn create<const N: usize>(
        table: &'static SlotTable<N>,
        iface: Iface,
        iids: &'static [Guid],
        data: T,
    ) -> *mut c_void {
        const {
            assert!(
                align_of::<T>() <= 8,
                "console fields must not need more than 8-byte alignment"
            )
        };
        let block = Box::into_raw(Box::new(Block {
            prefix: Prefix {
                refs: AtomicU32::new(1),
                iface,
                iids,
            },
            vtbl: table as *const SlotTable<N> as *const (),
            data,
        }));
        // SAFETY: `block` is a live allocation; the field address stays inside it.
        unsafe { std::ptr::addr_of_mut!((*block).vtbl) as *mut c_void }
    }

    /// The bookkeeping of the object the title points at.
    ///
    /// # Safety
    /// `obj` came from [`XObject::create`] and is still alive.
    pub unsafe fn prefix<'a>(obj: *mut c_void) -> &'a Prefix {
        &*(obj.cast::<u8>().sub(PREFIX_SIZE) as *const Prefix)
    }

    /// The console fields.
    ///
    /// # Safety
    /// As [`XObject::prefix`], and no `&mut` to the data exists.
    pub unsafe fn data<'a>(obj: *mut c_void) -> &'a T {
        &*(obj.cast::<u8>().add(8) as *const T)
    }

    /// The console fields, mutably.
    ///
    /// # Safety
    /// As [`XObject::prefix`], and no other reference to the data exists.
    pub unsafe fn data_mut<'a>(obj: *mut c_void) -> &'a mut T {
        &mut *(obj.cast::<u8>().add(8) as *mut T)
    }

    /// Adds a reference and returns the new count.
    ///
    /// # Safety
    /// As [`XObject::prefix`].
    pub unsafe fn add_ref(obj: *mut c_void) -> u32 {
        Self::prefix(obj).refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Drops a reference; at zero drops `T` and frees the allocation. Returns the new count.
    ///
    /// # Safety
    /// As [`XObject::prefix`]; the caller owns the reference.
    pub unsafe fn release(obj: *mut c_void) -> u32 {
        let left = Self::prefix(obj).refs.fetch_sub(1, Ordering::Release) - 1;
        if left == 0 {
            fence(Ordering::Acquire);
            drop(Box::from_raw(
                obj.cast::<u8>().sub(PREFIX_SIZE) as *mut Block<T>
            ));
        }
        left
    }
}

/// Slot 0: `IUnknown`, a listed IID returns the same pointer with a reference added.
///
/// # Safety
/// `this` is a live object holding a `T`; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn query_interface<T>(
    this: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if iid.is_null() {
        return E_POINTER;
    }
    let iid = *iid;
    if iid == IID_IUNKNOWN || XObject::<T>::prefix(this).iids.contains(&iid) {
        XObject::<T>::add_ref(this);
        *out = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// Slot 1.
///
/// # Safety
/// `this` is a live object holding a `T`.
pub unsafe extern "win64" fn add_ref<T>(this: *mut c_void) -> u32 {
    XObject::<T>::add_ref(this)
}

/// Slot 2.
///
/// # Safety
/// `this` is a live object holding a `T`; the caller owns a reference.
pub unsafe extern "win64" fn release<T>(this: *mut c_void) -> u32 {
    XObject::<T>::release(this)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    const IID_A: Guid = Guid::new(1, 2, 3, [4; 8]);
    const IID_B: Guid = Guid::new(9, 9, 9, [9; 8]);
    static IIDS: [Guid; 1] = [IID_A];

    #[repr(C)]
    struct Fields {
        a: u32,
        b: u32,
    }

    static TABLE: SlotTable<8> = SlotTable::new(Iface::Fence).with_iunknown::<Fields>();

    fn make() -> *mut c_void {
        XObject::create(
            &TABLE,
            Iface::Fence,
            &IIDS,
            Fields {
                a: 0x1234,
                b: 0x5678,
            },
        )
    }

    #[test]
    fn trap_message_text() {
        assert_eq!(
            trap_message(Iface::Device, 12),
            "nativ64: unimplemented ID3D12Device slot 12 (+0x60)"
        );
        assert_eq!(
            trap_message(Iface::Fence, 0),
            "nativ64: unimplemented ID3D12Fence slot 0 (+0x0)"
        );
    }

    #[test]
    fn iface_names_cover_registry() {
        for (i, f) in IFACES.iter().enumerate() {
            assert_eq!(*f as usize, i);
            assert!(f.name().starts_with("ID3D12"));
        }
    }

    #[test]
    fn table_overrides_and_distinct_traps() {
        assert_eq!(TABLE.slot(0), query_interface::<Fields> as *const ());
        assert_eq!(TABLE.slot(1), add_ref::<Fields> as *const ());
        assert_eq!(TABLE.slot(2), release::<Fields> as *const ());
        let dev = SlotTable::<8>::new(Iface::Device);
        assert_ne!(TABLE.slot(3), TABLE.slot(4));
        assert_ne!(TABLE.slot(3), dev.slot(3));
        assert_eq!(dev.slot(3), SlotTable::<4>::new(Iface::Device).slot(3));
        let big = SlotTable::<256>::new(Iface::CommandSignature);
        assert_ne!(big.slot(255), big.slot(254));
    }

    #[test]
    fn with_out_of_range_panics() {
        assert!(
            catch_unwind(|| SlotTable::<3>::new(Iface::Fence).with(3, std::ptr::null())).is_err()
        );
        assert!(catch_unwind(|| SlotTable::<257>::new(Iface::Fence)).is_err());
    }

    #[test]
    fn query_interface_cases() {
        let obj = make();
        unsafe {
            let mut out = std::ptr::null_mut();
            assert_eq!(
                query_interface::<Fields>(obj, &IID_IUNKNOWN, &mut out),
                S_OK
            );
            assert_eq!(out, obj);
            assert_eq!(query_interface::<Fields>(obj, &IID_A, &mut out), S_OK);
            assert_eq!(out, obj);
            assert_eq!(XObject::<Fields>::prefix(obj).ref_count(), 3);

            out = std::ptr::dangling_mut();
            assert_eq!(
                query_interface::<Fields>(obj, &IID_B, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(XObject::<Fields>::prefix(obj).ref_count(), 3);

            assert_eq!(
                query_interface::<Fields>(obj, &IID_A, std::ptr::null_mut()),
                E_POINTER
            );
            out = std::ptr::dangling_mut();
            assert_eq!(
                query_interface::<Fields>(obj, std::ptr::null(), &mut out),
                E_POINTER
            );
            assert!(out.is_null());

            for _ in 0..3 {
                release::<Fields>(obj);
            }
        }
    }

    #[test]
    fn refcounts_and_vtable_dispatch() {
        let obj = make();
        unsafe {
            assert_eq!(XObject::<Fields>::prefix(obj).ref_count(), 1);
            assert_eq!(XObject::<Fields>::prefix(obj).iface(), Iface::Fence);
            // Call through the vtable pointer the way the title does.
            let vtbl = *(obj as *const *const *const ());
            let add: unsafe extern "win64" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vtbl.add(1));
            let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vtbl.add(2));
            assert_eq!(add(obj), 2);
            assert_eq!(add_ref::<Fields>(obj), 3);
            assert_eq!(rel(obj), 2);
            assert_eq!(release::<Fields>(obj), 1);
            assert_eq!(release::<Fields>(obj), 0);
        }
    }

    #[test]
    fn release_frees_and_drops() {
        static DROPS: AtomicU32 = AtomicU32::new(0);
        struct Counted(u64);
        impl Drop for Counted {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        static T2: SlotTable<3> = SlotTable::new(Iface::Heap).with_iunknown::<Counted>();
        let obj = XObject::create(&T2, Iface::Heap, &[], Counted(7));
        unsafe {
            XObject::<Counted>::add_ref(obj);
            assert_eq!(XObject::<Counted>::release(obj), 1);
            assert_eq!(DROPS.load(Ordering::SeqCst), 0);
            assert_eq!(XObject::<Counted>::data(obj).0, 7);
            XObject::<Counted>::data_mut(obj).0 = 8;
            assert_eq!(XObject::<Counted>::release(obj), 0);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn data_at_plus_8_and_vtable_at_0() {
        let obj = make();
        unsafe {
            assert_eq!(*(obj as *const *const SlotTable<8>), &TABLE as *const _);
            assert_eq!(*(obj.cast::<u8>().add(8) as *const u32), 0x1234);
            assert_eq!(*(obj.cast::<u8>().add(12) as *const u32), 0x5678);
            let f = XObject::<Fields>::data(obj);
            assert_eq!((f.a, f.b), (0x1234, 0x5678));
            release::<Fields>(obj);
        }
    }
}
