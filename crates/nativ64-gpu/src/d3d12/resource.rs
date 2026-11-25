//! Resources (`ID3D12Resource`). Memory is unified: a resource is a description over title
//! memory, and its GPU address is that memory's CPU address.
//!
//! First kind: `CreateResourceAtAddressX`-style placement (device slot 133), a resource at an
//! address the title mapped itself. Committed and heap-placed resources come with their slots.

use std::ffi::c_void;

use crate::d3d12::device::{E_INVALIDARG, E_OUTOFMEMORY};
use crate::d3d12::info::HeapProperties;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::texture::Layout;
use crate::xobj::{Guid, Iface, SlotTable, HRESULT, S_OK};

pub static RESOURCE_IIDS: [Guid; 4] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// `D3D12_RESOURCE_DESC`, as the title passes it (the newer description starts the same and
/// adds a sampler feedback mip region, ignored here).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array_size: u16,
    pub mip_levels: u16,
    pub format: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub layout: u32,
    pub flags: u32,
}
const _: () = assert!(size_of::<ResourceDesc>() == 56);

/// Console fields from `+0x08` (none are read by inlined code that we know of); then ours.
#[repr(C)]
pub struct ResourceData {
    bits: u32,
    console_refs: u32,
    /// The memory the resource covers; also its GPU address.
    pub address: u64,
    pub desc: ResourceDesc,
    /// Initial barrier layout (`-1` when the creation call gives none).
    pub layout: u32,
    /// Committed resources own their memory (released with the resource), taken with these
    /// `XMemAlloc` attributes; 0 for memory the title owns.
    owned: u64,
}

impl Drop for ResourceData {
    fn drop(&mut self) {
        if self.owned != 0 {
            crate::xmem::free(self.address as *mut c_void, self.owned);
        }
    }
}

static TABLE: SlotTable<32> =
    with_child_methods(SlotTable::new(Iface::Resource).with_iunknown::<ResourceData>())
        .with(8, map as *const ())
        .with(9, unmap as *const ())
        .with(10, get_desc as *const ())
        .with(11, get_gpu_virtual_address as *const ());

/// `ID3D12Device::CreateResourceAtAddressX(address, desc1, layout, clear, riid, out)` (device
/// slot 133): a resource over memory the title mapped itself. The clear value is not kept.
/// Shortcut: the address is not checked against the memory tables.
///
/// # Safety
/// `desc` is NULL or a description; `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_at_address(
    _this: *mut c_void,
    address: u64,
    desc: *const ResourceDesc,
    layout: u32,
    _clear: *const c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if address == 0 || desc.is_null() {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    }
    let desc = desc.read_unaligned();
    hand_out(
        &TABLE,
        Iface::Resource,
        &RESOURCE_IIDS,
        || ResourceData {
            bits: 0,
            console_refs: 0,
            address,
            desc,
            layout,
            owned: 0,
        },
        iid,
        out,
    )
}

/// Shared by the committed creation slots: zeroed memory sized and aligned by `footprint`, at
/// an address that is the resource's GPU address. Shortcut: the heap properties and flags are
/// not used (all memory is the same host memory) and the clear value is not kept.
unsafe fn create_committed(
    heap: *const HeapProperties,
    desc: *const ResourceDesc,
    layout: u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if heap.is_null() || desc.is_null() {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        return E_INVALIDARG;
    }
    let desc = desc.read_unaligned();
    let info = match footprint(&desc) {
        Ok(info) => info,
        Err(why) => {
            eprintln!("nativ64: no size for committed resource {desc:?}: {why}");
            if !out.is_null() {
                *out = std::ptr::null_mut();
            }
            return E_INVALIDARG;
        }
    };
    if out.is_null() {
        // only asks whether creation would work
        return hand_out(
            &TABLE,
            Iface::Resource,
            &RESOURCE_IIDS,
            || -> ResourceData { unreachable!() },
            iid,
            out,
        );
    }
    let Ok(size) = usize::try_from(info.size) else {
        *out = std::ptr::null_mut();
        return E_INVALIDARG;
    };
    // The memory comes from `XMemAlloc`, so the title's allocation hook (its own pool for
    // graphics memory) sees the request as it would on the console.
    let attributes = crate::xmem::GPU_ALLOC_ATTRIBUTES
        | u64::from(info.alignment.max(KIB_64).trailing_zeros()) << 24;
    let address = crate::xmem::alloc(size, attributes);
    if address.is_null() {
        eprintln!("nativ64: no memory for committed resource {desc:?}");
        *out = std::ptr::null_mut();
        return E_OUTOFMEMORY;
    }
    let hr = hand_out(
        &TABLE,
        Iface::Resource,
        &RESOURCE_IIDS,
        || ResourceData {
            bits: 0,
            console_refs: 0,
            address: address as u64,
            desc,
            layout,
            owned: attributes,
        },
        iid,
        out,
    );
    if hr != S_OK {
        crate::xmem::free(address, attributes);
    }
    hr
}

/// `ID3D12Device::CreateCommittedResource(heap, heap_flags, desc, state, clear, riid, out)`
/// (device slot 27). The initial state is not tracked (layout `-1`).
///
/// # Safety
/// Pointers are NULL or valid.
pub unsafe extern "win64" fn create_committed_resource(
    _this: *mut c_void,
    heap: *const HeapProperties,
    _heap_flags: u32,
    desc: *const ResourceDesc,
    _state: u32,
    _clear: *const c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    create_committed(heap, desc, u32::MAX, iid, out)
}

/// `CreateCommittedResource3(heap, heap_flags, desc1, initial_layout, clear, protected_session,
/// castable_count, castable_formats, riid, out)` (device slot 127, `+0x3f8`).
///
/// # Safety
/// Pointers are NULL or valid.
pub unsafe extern "win64" fn create_committed_resource3(
    _this: *mut c_void,
    heap: *const HeapProperties,
    _heap_flags: u32,
    desc: *const ResourceDesc,
    layout: u32,
    _clear: *const c_void,
    _protected_session: *mut c_void,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    create_committed(heap, desc, layout, iid, out)
}

const BUFFER: u32 = 1;
const KIB_64: u64 = 64 << 10;

/// `D3D12_RESOURCE_ALLOCATION_INFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocationInfo {
    pub size: u64,
    pub alignment: u64,
}

/// What a resource takes in memory: buffers their width rounded up to 64 KiB, textures what
/// their console layout takes (`texture::Layout`). The error says why there is no known size
/// (zero or overflowing sizes, a bad alignment, a texture we cannot lay out); the API itself
/// cannot fail, it reports the documented size `u64::MAX`.
fn footprint(desc: &ResourceDesc) -> Result<AllocationInfo, String> {
    if desc.dimension != BUFFER {
        let layout = Layout::new(desc).map_err(|e| e.to_string())?;
        return Ok(AllocationInfo {
            size: layout.size,
            alignment: layout.alignment,
        });
    }
    if desc.width == 0 {
        return Err("zero width".into());
    }
    let alignment = desc.alignment.max(KIB_64);
    alignment
        .is_power_of_two()
        .then_some(())
        .ok_or("alignment is not a power of two")?;
    Ok(AllocationInfo {
        size: desc
            .width
            .checked_next_multiple_of(KIB_64)
            .ok_or("size overflows")?,
        alignment,
    })
}

/// The footprint of resources placed one after another; an unknown one gives the documented
/// size `u64::MAX`.
fn placed_together(descs: &[ResourceDesc]) -> AllocationInfo {
    let unknown = AllocationInfo {
        size: u64::MAX,
        alignment: 0,
    };
    let mut total = AllocationInfo {
        size: 0,
        alignment: KIB_64,
    };
    for desc in descs {
        let one = match footprint(desc) {
            Ok(one) => one,
            Err(why) => {
                eprintln!("nativ64: no allocation size for resource {desc:?}: {why}");
                return unknown;
            }
        };
        let Some(offset) = total.size.checked_next_multiple_of(one.alignment) else {
            return unknown;
        };
        total.size = offset.saturating_add(one.size);
        total.alignment = total.alignment.max(one.alignment);
    }
    total
}

/// `ID3D12Device::GetResourceAllocationInfo(visible_mask, count, descs)` (device slot 25). The
/// 16-byte result comes through the hidden pointer, passed first (`this` second).
///
/// # Safety
/// `out` is NULL or has room for the result; `descs` is NULL or holds `count` descriptions.
pub unsafe extern "win64" fn get_resource_allocation_info(
    out: *mut AllocationInfo,
    _this: *mut c_void,
    _visible_mask: u32,
    count: u32,
    descs: *const ResourceDesc,
) -> *mut AllocationInfo {
    let descs = if descs.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(descs, count as usize)
    };
    if !out.is_null() {
        out.write(placed_together(descs));
    }
    out
}

/// Slot 8, `Map(subresource, read_range, out)`: memory is always mapped; a NULL `out` only asks
/// for residency.
unsafe extern "win64" fn map(
    this: *mut c_void,
    _subresource: u32,
    _read: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = crate::xobj::XObject::<ResourceData>::data(this).address as *mut c_void;
    }
    S_OK
}

/// Slot 9, `Unmap(subresource, written_range)`.
unsafe extern "win64" fn unmap(_this: *mut c_void, _subresource: u32, _written: *const c_void) {}

/// Slot 10, `GetDesc()`: returns a struct, so the return pointer comes first and `this` second.
unsafe extern "win64" fn get_desc(out: *mut ResourceDesc, this: *mut c_void) -> *mut ResourceDesc {
    if !out.is_null() {
        out.write(crate::xobj::XObject::<ResourceData>::data(this).desc);
    }
    out
}

/// Slot 11, `GetGPUVirtualAddress()`.
unsafe extern "win64" fn get_gpu_virtual_address(this: *mut c_void) -> u64 {
    crate::xobj::XObject::<ResourceData>::data(this).address
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{XObject, E_NOINTERFACE};

    fn buffer(width: u64) -> ResourceDesc {
        ResourceDesc {
            dimension: 1,
            width,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            sample_count: 1,
            layout: 1,
            ..Default::default()
        }
    }

    #[test]
    fn create_at_address_and_methods() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = buffer(0x1000);
            let mut r = n;
            assert_eq!(
                create_at_address(
                    n,
                    0x10b0_0000,
                    &desc,
                    u32::MAX,
                    std::ptr::null(),
                    &RESOURCE_IIDS[0],
                    &mut r
                ),
                S_OK
            );
            assert_eq!(get_gpu_virtual_address(r), 0x10b0_0000);
            let mut mapped = n;
            assert_eq!(map(r, 0, std::ptr::null(), &mut mapped), S_OK);
            assert_eq!(mapped as u64, 0x10b0_0000);
            assert_eq!(map(r, 0, std::ptr::null(), std::ptr::null_mut()), S_OK);
            unmap(r, 0, std::ptr::null());
            let mut got = ResourceDesc::default();
            assert_eq!(get_desc(&mut got, r), &mut got as *mut _);
            assert_eq!(got, desc);
            // through the vtable, as the title calls it
            let vtbl = *(r as *const *const *const ());
            let f: unsafe extern "win64" fn(*mut c_void) -> u64 =
                std::mem::transmute(*vtbl.add(11));
            assert_eq!(f(r), 0x10b0_0000);
            assert_eq!(XObject::<ResourceData>::data(r).layout, u32::MAX);
            assert_eq!(XObject::<ResourceData>::release(r), 0);
        }
    }

    #[test]
    fn allocation_info_places_resources_in_order() {
        unsafe {
            let n = std::ptr::null_mut();
            let mut info = AllocationInfo {
                size: 0,
                alignment: 0,
            };
            let one = [buffer(100)];
            assert_eq!(
                get_resource_allocation_info(&mut info, n, 1, 1, one.as_ptr()),
                &mut info as *mut _
            );
            assert_eq!(
                info,
                AllocationInfo {
                    size: 65536,
                    alignment: 65536
                }
            );
            let mut big = buffer(65537);
            big.alignment = 1 << 22;
            let two = [buffer(1), big, buffer(70000)];
            get_resource_allocation_info(&mut info, n, 1, 3, two.as_ptr());
            // 64K, then 4 MiB aligned (+128K), then 128K at the next 64K
            assert_eq!(
                info,
                AllocationInfo {
                    size: (4 << 20) + 131072 + 131072,
                    alignment: 1 << 22
                }
            );
            // nothing: an empty placement
            get_resource_allocation_info(&mut info, n, 1, 0, std::ptr::null());
            assert_eq!(
                info,
                AllocationInfo {
                    size: 0,
                    alignment: 65536
                }
            );
            // unknown: zero width, a texture, bad alignment, overflow
            let mut bad = buffer(16);
            bad.alignment = 100_000;
            let mut huge = buffer(u64::MAX);
            huge.alignment = 0;
            let mut texture = buffer(16);
            texture.dimension = 3;
            for d in [buffer(0), texture, bad, huge] {
                get_resource_allocation_info(&mut info, n, 1, 1, &d);
                assert_eq!(
                    info,
                    AllocationInfo {
                        size: u64::MAX,
                        alignment: 0
                    },
                    "{d:?}"
                );
            }
            assert!(
                get_resource_allocation_info(std::ptr::null_mut(), n, 1, 0, std::ptr::null())
                    .is_null()
            );
        }
    }

    #[test]
    fn allocation_info_of_textures() {
        unsafe {
            let n = std::ptr::null_mut();
            let mut info = AllocationInfo {
                size: 0,
                alignment: 0,
            };
            // the reference render target, with a buffer placed after it
            let rt = ResourceDesc {
                dimension: 3,
                width: 3840,
                height: 2160,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: 28,
                sample_count: 1,
                flags: 0x5,
                ..Default::default()
            };
            get_resource_allocation_info(&mut info, n, 1, 1, &rt);
            assert_eq!(
                info,
                AllocationInfo {
                    size: 33_521_664,
                    alignment: 65536
                }
            );
            let two = [rt, buffer(1)];
            get_resource_allocation_info(&mut info, n, 1, 2, two.as_ptr());
            assert_eq!(info.size, 512 * 65536 + 65536);
            // a small texture keeps its block alignment; the group stays 64 KiB aligned
            let small = ResourceDesc {
                width: 16,
                height: 16,
                flags: 0,
                ..rt
            };
            get_resource_allocation_info(&mut info, n, 1, 1, &small);
            assert_eq!(
                info,
                AllocationInfo {
                    size: 4096,
                    alignment: 65536
                }
            );
            // MSAA and VAR layouts have no size
            for bad in [
                ResourceDesc {
                    sample_count: 4,
                    ..rt
                },
                ResourceDesc {
                    layout: 0x100 | 28,
                    ..rt
                },
            ] {
                get_resource_allocation_info(&mut info, n, 1, 1, &bad);
                assert_eq!(info.size, u64::MAX);
            }
        }
    }

    #[test]
    fn create_errors() {
        unsafe {
            let n = std::ptr::null_mut();
            let desc = buffer(16);
            let mut r = std::ptr::dangling_mut::<c_void>();
            let iid = &RESOURCE_IIDS[0];
            assert_eq!(
                create_at_address(n, 0, &desc, 0, std::ptr::null(), iid, &mut r),
                E_INVALIDARG
            );
            assert!(r.is_null());
            assert_eq!(
                create_at_address(
                    n,
                    0x1000,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    iid,
                    &mut r
                ),
                E_INVALIDARG
            );
            assert_eq!(
                create_at_address(
                    n,
                    0x1000,
                    &desc,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut r
                ),
                E_INVALIDARG
            );
            let bad = guid(1, 2, 3, [0; 8]);
            assert_eq!(
                create_at_address(n, 0x1000, &desc, 0, std::ptr::null(), &bad, &mut r),
                E_NOINTERFACE
            );
            assert!(r.is_null());
            // NULL out only asks whether creation would work
            assert_eq!(
                create_at_address(
                    n,
                    0x1000,
                    &desc,
                    0,
                    std::ptr::null(),
                    iid,
                    std::ptr::null_mut()
                ),
                S_FALSE
            );
            assert_eq!(
                create_at_address(n, 0, &desc, 0, std::ptr::null(), iid, std::ptr::null_mut()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn committed_resources() {
        unsafe {
            let n = std::ptr::null_mut();
            let heap = HeapProperties {
                kind: 1,
                cpu_page_property: 0,
                memory_pool: 0,
                creation_node_mask: 0,
                visible_node_mask: 0,
            };
            let desc = buffer(100);
            let iid = &RESOURCE_IIDS[0];
            let mut r = n;
            assert_eq!(
                create_committed_resource3(
                    n,
                    &heap,
                    0,
                    &desc,
                    5,
                    std::ptr::null(),
                    n,
                    0,
                    std::ptr::null(),
                    iid,
                    &mut r
                ),
                S_OK
            );
            let address = get_gpu_virtual_address(r);
            assert_eq!(address % 65536, 0);
            assert!(address >= 1 << 32);
            assert_eq!(*(address as *const u8), 0);
            assert_eq!(XObject::<ResourceData>::data(r).layout, 5);
            let mut r2 = n;
            assert_eq!(
                create_committed_resource(n, &heap, 0, &desc, 0, std::ptr::null(), iid, &mut r2),
                S_OK
            );
            assert_ne!(get_gpu_virtual_address(r2), address);
            assert_eq!(XObject::<ResourceData>::data(r2).layout, u32::MAX);
            assert_eq!(XObject::<ResourceData>::release(r), 0);
            assert_eq!(XObject::<ResourceData>::release(r2), 0);
            // errors: null heap or desc, unknown layout (zero width), null out
            let zero = buffer(0);
            let mut bad = std::ptr::dangling_mut::<c_void>();
            for (h, d) in [
                (std::ptr::null(), &desc as *const _),
                (&heap as *const _, std::ptr::null()),
                (&heap as *const _, &zero as *const _),
            ] {
                bad = std::ptr::dangling_mut::<c_void>();
                assert_eq!(
                    create_committed_resource(n, h, 0, d, 0, std::ptr::null(), iid, &mut bad),
                    E_INVALIDARG
                );
                assert!(bad.is_null());
            }
            let _ = bad;
            assert_eq!(
                create_committed_resource(
                    n,
                    &heap,
                    0,
                    &desc,
                    0,
                    std::ptr::null(),
                    iid,
                    std::ptr::null_mut()
                ),
                S_FALSE
            );
        }
    }
}
