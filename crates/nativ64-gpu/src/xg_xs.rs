//! `xg_xs.dll`: the console's texture layout library. A title asks it which swizzle mode a
//! texture should have (`XGSuggestSwizzleMode`), and makes a "texture computer" for a
//! description to learn the size, where each mip and slice lies, and to copy linear data into
//! tiled memory and back. The layouts come from `texture::Layout`; see `docs/textures.md`.
//!
//! The computer is an object without `QueryInterface`: slot 0 is `AddRef`, slot 1 `Release`,
//! then the methods in the order of `Slot`. Its console-visible fields are none (the title only
//! calls methods).

use std::ffi::c_void;
use std::mem::offset_of;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::resource::ResourceDesc;
use crate::texture::layout::{MetaKind, PlaneKind};
use crate::texture::{detile, format_info, suggest_mode, tile, Addresser, Layout, TileError};
use crate::xobj::{add_ref, release, Iface, SlotTable, XObject, HRESULT, S_OK};
use nativ64::com::{E_FAIL, E_NOTIMPL};

/// `0x100 | mode` in a description's layout field: an explicit swizzle mode.
const EXPLICIT: u32 = 0x100;
const TILE_BYTES: u64 = 1 << 16;
const MAX_MIPS: usize = 15;

/// One mip level of `XG_RESOURCE_LAYOUT` (0x70 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MipOut {
    pub size: u64,
    /// From the start of slice 0 of the plane.
    pub offset: u64,
    /// Bytes of one slice (a depth slice for 3D).
    pub slice_size: u64,
    pub pitch_texels: u32,
    pub pitch_bytes: u32,
    pub alignment: u32,
    /// Padded width, height (elements) and depth (or array size).
    pub padded: [u32; 3],
    /// Width and height in elements, then in texels.
    pub elements: [u32; 2],
    pub texels: [u32; 2],
    pub depth_or_array: u32,
    pub samples: u32,
    pub swizzle: u32,
    pub _pad0: u32,
    /// 0x8000 for the levels of the Z, S_X, D_X and R_X modes outside the tail.
    pub bank_rotation: [u64; 2],
    /// Depth of a block (1 unless thick).
    pub block_depth: u32,
    pub in_tail: u32,
    /// Position of a thick tail level in its block: x in bits 0-10, y in 11-21.
    pub tail_position: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlaneOut {
    /// 1 colour, 2 CMASK, 4 HTILE, 5 and 6 the planes of a depth format used as colour, 7
    /// depth, 8 stencil.
    pub usage: u32,
    pub _pad0: u32,
    pub size: u64,
    pub slice_size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub element_bytes: u32,
    pub first_tail_mip: u32,
    pub tail_mips: u32,
    pub _pad1: u32,
    pub mips: [MipOut; MAX_MIPS],
}

/// `XG_RESOURCE_LAYOUT`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LayoutOut {
    pub size: u64,
    pub alignment: u64,
    pub mips: u32,
    pub planes: u32,
    pub plane: [PlaneOut; 4],
    pub dimension: u32,
    pub _pad: u32,
}

const _: () = {
    assert!(size_of::<MipOut>() == 0x70);
    assert!(offset_of!(MipOut, pitch_texels) == 0x18);
    assert!(offset_of!(MipOut, alignment) == 0x20);
    assert!(offset_of!(MipOut, elements) == 0x30);
    assert!(offset_of!(MipOut, texels) == 0x38);
    assert!(offset_of!(MipOut, samples) == 0x44);
    assert!(offset_of!(MipOut, swizzle) == 0x48);
    assert!(offset_of!(MipOut, bank_rotation) == 0x50);
    assert!(offset_of!(MipOut, block_depth) == 0x60);
    assert!(offset_of!(MipOut, in_tail) == 0x64);
    assert!(offset_of!(MipOut, tail_position) == 0x68);
    assert!(size_of::<PlaneOut>() == 0x6C8);
    assert!(offset_of!(PlaneOut, offset) == 0x18);
    assert!(offset_of!(PlaneOut, alignment) == 0x20);
    assert!(offset_of!(PlaneOut, element_bytes) == 0x28);
    assert!(offset_of!(PlaneOut, first_tail_mip) == 0x2C);
    assert!(offset_of!(PlaneOut, mips) == 0x38);
    assert!(offset_of!(LayoutOut, plane) == 0x18);
    assert!(offset_of!(LayoutOut, dimension) == 0x1B38);
    assert!(size_of::<LayoutOut>() == 0x1B40);
};

/// D3D12's `D3D12_PACKED_MIP_INFO`.
#[repr(C)]
pub struct PackedMips {
    standard: u8,
    packed: u8,
    tiles: u32,
    start: u32,
}

/// D3D12's `D3D12_SUBRESOURCE_TILING`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SubresourceTiling {
    width: u32,
    height: u16,
    depth: u16,
    start: u32,
}

/// What a computer holds: the description and its layout.
pub struct Computer {
    desc: ResourceDesc,
    layout: Layout,
}

type This = *mut c_void;

/// The methods, by slot (after `AddRef` and `Release`).
#[allow(dead_code)]
mod slot {
    pub const GET_RESOURCE_LAYOUT: usize = 2;
    pub const SIZE: usize = 3;
    pub const ALIGNMENT: usize = 4;
    pub const MIP_OFFSET: usize = 5;
    pub const TEXEL_OFFSET: usize = 6;
    pub const TEXEL_COORDINATE: usize = 7;
    pub const COPY_INTO: usize = 8;
    pub const COPY_FROM: usize = 9;
    pub const RESOURCE_TILING: usize = 10;
}

static TABLE: SlotTable<13> = SlotTable::new(Iface::XgTextureComputer)
    .with(0, add_ref::<Computer> as *const ())
    .with(1, release::<Computer> as *const ())
    .with(slot::GET_RESOURCE_LAYOUT, get_resource_layout as *const ())
    .with(slot::SIZE, get_size as *const ())
    .with(slot::ALIGNMENT, get_alignment as *const ())
    .with(slot::MIP_OFFSET, get_mip_offset as *const ())
    .with(slot::TEXEL_OFFSET, get_texel_offset as *const ())
    .with(slot::TEXEL_COORDINATE, not_implemented as *const ())
    .with(slot::COPY_INTO, copy_into as *const ())
    .with(slot::COPY_FROM, copy_from as *const ())
    .with(slot::RESOURCE_TILING, get_resource_tiling as *const ())
    .with(11, not_implemented as *const ())
    .with(12, not_implemented as *const ());

/// The description at `desc`, or `None` for null.
///
/// # Safety
/// `desc` is null or points at a `D3D12_RESOURCE_DESC`.
unsafe fn read_desc(desc: *const c_void) -> Option<ResourceDesc> {
    (desc as *const ResourceDesc).as_ref().copied()
}

/// `XGSuggestSwizzleMode(desc, flags, out)`: writes `0x100 | mode` for the description.
///
/// # Safety
/// The pointers are null or valid.
unsafe fn suggest(desc: *const c_void, _flags: u32, out: *mut u32) -> HRESULT {
    let (Some(desc), Some(out)) = (read_desc(desc), out.as_mut()) else {
        return E_INVALIDARG;
    };
    *out = EXPLICIT | suggest_mode(&desc).0;
    S_OK
}

/// `XGCreateTextureComputer(desc, out)`.
///
/// # Safety
/// The pointers are null or valid.
unsafe fn create(desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let (Some(desc), false) = (read_desc(desc), out.is_null()) else {
        return E_INVALIDARG;
    };
    *out = std::ptr::null_mut();
    match Layout::new(&desc) {
        Ok(layout) => {
            *out = XObject::create(
                &TABLE,
                Iface::XgTextureComputer,
                &[],
                Computer { desc, layout },
            );
            S_OK
        }
        Err(e) => {
            eprintln!("nativ64: XGCreateTextureComputer: {e}");
            e.hresult()
        }
    }
}

nativ64::win32! {
    module "xg_xs";

    Native fn XGSuggestSwizzleMode(desc: *const c_void, flags: u32, out: *mut u32) -> i32 {
        // SAFETY: the title passes a description and room for the layout; null is checked.
        unsafe { suggest(desc, flags, out) }
    }

    Native fn XGCreateTextureComputer(desc: *const c_void, out: *mut *mut c_void) -> i32 {
        // SAFETY: as above.
        unsafe { create(desc, out) }
    }
}

/// # Safety
/// `this` is a live computer.
unsafe fn computer<'a>(this: This) -> &'a Computer {
    XObject::<Computer>::data(this)
}

fn element_texels(desc: &ResourceDesc) -> u32 {
    format_info(desc.format).map_or(1, |f| f.block.0)
}

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

/// Depth (or array size) of level `mip` as the library reports it, and padded.
fn mip_slices(c: &Computer, mip: usize) -> (u32, u32) {
    let l = &c.layout;
    if l.dimension != 4 {
        return (l.depth_or_array, l.depth_or_array);
    }
    let d = (l.depth_or_array >> mip).max(1);
    let block_depth = l.planes[0].block[2];
    if l.mode.is_linear() {
        (d, l.depth_or_array)
    } else if !l.thick && l.mip_levels > 1 {
        (d, 1)
    } else {
        (
            d,
            align_up(l.depth_or_array.div_ceil(1 << mip), block_depth),
        )
    }
}

fn mip_out(c: &Computer, plane: usize, i: usize) -> MipOut {
    let (l, p) = (&c.layout, &c.layout.planes[plane]);
    let m = &p.mips[i];
    let side = element_texels(&c.desc);
    let e = u64::from(p.element_bytes);
    let slice = u64::from(m.pitch) * u64::from(m.rows) * e;
    let (depth, padded_depth) = mip_slices(c, i);
    let tail = m.tail;
    let mode = l.mode.0;
    let mut out = MipOut {
        size: if l.dimension == 4 {
            slice * u64::from(padded_depth)
        } else {
            slice
        },
        offset: m.offset,
        slice_size: slice,
        pitch_texels: m.pitch * side,
        pitch_bytes: m.pitch * p.element_bytes,
        alignment: if tail.is_some() {
            0x100
        } else {
            l.mode.block_bytes().max(256) as u32
        },
        padded: [m.pitch, m.rows, padded_depth],
        elements: [m.width, m.height],
        texels: [
            ((c.desc.width >> i) as u32).max(1),
            (c.desc.height >> i).max(1),
        ],
        depth_or_array: depth,
        samples: 1,
        // tail levels are laid out in 256-byte blocks of the same kind (S 1, D 2, R 3)
        swizzle: if tail.is_some() && mode % 4 != 0 {
            mode % 4
        } else {
            mode
        },
        block_depth: p.block[2],
        in_tail: u32::from(tail.is_some()),
        ..MipOut::default()
    };
    if tail.is_none() && (24..=27).contains(&mode) {
        out.bank_rotation = [0x8000, 0x8000];
    }
    if let (Some(t), true) = (tail, l.thick) {
        out.tail_position = t.x | t.y << 11;
    }
    out
}

fn fill_layout(c: &Computer) -> LayoutOut {
    let l = &c.layout;
    // SAFETY: every field is plain data; zeroes are a valid value.
    let mut out: LayoutOut = unsafe { std::mem::zeroed() };
    out.size = l.size;
    out.alignment = l.alignment;
    out.mips = l.mip_levels;
    out.planes = (l.planes.len() + l.metadata.len()) as u32;
    out.dimension = l.dimension;
    let depth_buffer = c.desc.flags & 2 != 0 && (l.planes.len() > 1 || l.mip_levels == 1);
    let two = l.planes.len() > 1;
    for (k, p) in l.planes.iter().enumerate() {
        let usage = match (p.kind, depth_buffer, two) {
            (PlaneKind::Stencil, true, _) => 8,
            (PlaneKind::Stencil, false, _) => 6,
            (PlaneKind::Main, true, _) => 7,
            (PlaneKind::Main, false, true) => 5,
            (PlaneKind::Main, false, false) => 1,
        };
        let first_tail = p
            .mips
            .iter()
            .position(|m| m.tail.is_some())
            .unwrap_or(p.mips.len());
        let o = &mut out.plane[k];
        o.usage = usage;
        o.size = p.size;
        o.slice_size = p.slice_size;
        o.offset = p.offset;
        o.alignment = l.mode.block_bytes().max(256);
        o.element_bytes = p.element_bytes;
        o.first_tail_mip = first_tail as u32;
        o.tail_mips = (p.mips.len() - first_tail) as u32;
        for (i, slot) in o.mips.iter_mut().enumerate().take(p.mips.len()) {
            *slot = mip_out(c, k, i);
        }
    }
    for (k, m) in l.metadata.iter().enumerate() {
        let o = &mut out.plane[l.planes.len() + k];
        let (usage, align) = match m.kind {
            MetaKind::Cmask => (2, 0x2000),
            MetaKind::Htile => (4, TILE_BYTES),
        };
        o.usage = usage;
        o.size = m.size;
        o.offset = m.offset;
        o.alignment = align;
        o.mips[0].size = m.size;
        o.mips[0].alignment = align as u32;
    }
    out
}

unsafe extern "win64" fn get_resource_layout(this: This, out: *mut LayoutOut) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    out.write(fill_layout(computer(this)));
    S_OK
}

unsafe extern "win64" fn get_size(this: This) -> u64 {
    computer(this).layout.size
}

unsafe extern "win64" fn get_alignment(this: This) -> u64 {
    computer(this).layout.alignment
}

/// From the start of the resource; 0 for a plane or mip that does not exist.
unsafe extern "win64" fn get_mip_offset(this: This, plane: u32, mip: u32) -> u64 {
    let l = &computer(this).layout;
    l.planes
        .get(plane as usize)
        .and_then(|p| Some(p.offset + p.mips.get(mip as usize)?.offset))
        .unwrap_or(0)
}

/// `(plane, mip, x, y, z or slice, sample, *bit)`: the byte offset of an element from the
/// start of the resource (the bit position inside it is always 0). 0 outside the texture.
unsafe extern "win64" fn get_texel_offset(
    this: This,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    layer: u32,
    _sample: u32,
    bit: *mut u32,
) -> u64 {
    if let Some(bit) = bit.as_mut() {
        *bit = 0;
    }
    let l = &computer(this).layout;
    let (plane, mip) = (plane as usize, mip as usize);
    let Some(m) = l.planes.get(plane).and_then(|p| p.mips.get(mip)) else {
        return 0;
    };
    let depth = mip_slices(computer(this), mip).0;
    let Ok(x) = u32::try_from(x) else { return 0 };
    if x >= m.width || y >= m.height || layer >= depth {
        return 0;
    }
    Addresser::new(l, plane, mip, 0).map_or(0, |a| a.offset(x, y, layer))
}

unsafe extern "win64" fn not_implemented(_this: This) -> HRESULT {
    eprintln!("nativ64: XGTextureAddressComputer: method not implemented");
    E_NOTIMPL
}

fn tile_hresult(e: TileError) -> HRESULT {
    match e {
        TileError::Equation(_) | TileError::Multisampled => E_NOTIMPL,
        _ => E_INVALIDARG,
    }
}

/// `CopyIntoSubresource(tiled, plane, subresource, linear, row pitch, slice pitch)`: `tiled`
/// is the start of the whole resource.
unsafe extern "win64" fn copy_into(
    this: This,
    tiled: *mut u8,
    plane: u32,
    subresource: u32,
    linear: *const u8,
    row_pitch: u32,
    slice_pitch: u32,
) -> HRESULT {
    let l = &computer(this).layout;
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let Some(len) = linear_len(l, plane, subresource, row_pitch, slice_pitch) else {
        return E_INVALIDARG;
    };
    // The title may free either range from another thread: copy instead of slicing them.
    let Some(linear) = crate::hostmem::peek(linear as u64, len) else {
        return E_INVALIDARG;
    };
    let tiled_addr = tiled as u64;
    let Some(mut tiled) = crate::hostmem::peek(tiled_addr, l.size as usize) else {
        return E_INVALIDARG;
    };
    match tile(
        l,
        plane as usize,
        subresource,
        0,
        &mut tiled,
        &linear,
        (row_pitch as usize, slice_pitch as usize),
    ) {
        Ok(()) => {
            if crate::hostmem::poke(tiled_addr, &tiled) {
                S_OK
            } else {
                E_INVALIDARG
            }
        }
        Err(e) => tile_hresult(e),
    }
}

/// `CopyFromSubresource(linear, plane, subresource, tiled, row pitch, slice pitch)`. VIctory
/// has one more argument (a sample) before `tiled`; XCL, checked against the console's
/// library, does not, and agrees with `CopyIntoSubresource`.
unsafe extern "win64" fn copy_from(
    this: This,
    linear: *mut u8,
    plane: u32,
    subresource: u32,
    tiled: *const u8,
    row_pitch: u32,
    slice_pitch: u32,
) -> HRESULT {
    let l = &computer(this).layout;
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let Some(len) = linear_len(l, plane, subresource, row_pitch, slice_pitch) else {
        return E_INVALIDARG;
    };
    // The title may free either range from another thread: copy instead of slicing them.
    let Some(tiled) = crate::hostmem::peek(tiled as u64, l.size as usize) else {
        return E_INVALIDARG;
    };
    let linear_addr = linear as u64;
    let mut linear = vec![0u8; len];
    match detile(
        l,
        plane as usize,
        subresource,
        0,
        &tiled,
        &mut linear,
        (row_pitch as usize, slice_pitch as usize),
    ) {
        Ok(()) => {
            if crate::hostmem::poke(linear_addr, &linear) {
                S_OK
            } else {
                E_INVALIDARG
            }
        }
        Err(e) => tile_hresult(e),
    }
}

/// How many bytes of the title's linear memory a copy of the subresource touches.
fn linear_len(l: &Layout, plane: u32, subresource: u32, row: u32, slice: u32) -> Option<usize> {
    let p = l.planes.get(plane as usize)?;
    let mip = p.mips.get((subresource % l.mip_levels) as usize)?;
    let depth = if l.dimension == 4 {
        if l.thick {
            mip.depth
        } else {
            (l.depth_or_array >> (subresource % l.mip_levels)).max(1)
        }
    } else {
        1
    };
    let bytes = mip.width as usize * p.element_bytes as usize;
    Some((depth as usize - 1) * slice as usize + (mip.height as usize - 1) * row as usize + bytes)
}

/// `GetResourceTiling(*tiles, *packed, *shape, *count, first, *out)`: D3D12's call for the
/// 64 KiB tiles of plane 0. Only the 64 KiB 2D modes answer (others `E_FAIL`, volumes
/// `E_NOTIMPL`). The tail is tile 0 of each slice. `count` goes in as room and comes back as
/// the number of standard (not packed) subresources among the asked ones; each is written at
/// its own index from `first`, the packed ones are left untouched.
unsafe extern "win64" fn get_resource_tiling(
    this: This,
    tiles: *mut u32,
    packed: *mut PackedMips,
    shape: *mut [u32; 3],
    count: *mut u32,
    first: u32,
    out: *mut SubresourceTiling,
) -> HRESULT {
    let c = computer(this);
    let l = &c.layout;
    let p = &l.planes[0];
    if l.mode.block_bytes() != TILE_BYTES || l.mode.is_linear() {
        return E_FAIL;
    }
    if l.dimension == 4 {
        return E_NOTIMPL;
    }
    let mips = p.mips.len() as u32;
    let standard = p
        .mips
        .iter()
        .position(|m| m.tail.is_some())
        .unwrap_or(p.mips.len()) as u32;
    let side = element_texels(&c.desc);
    if let Some(t) = tiles.as_mut() {
        *t = l.size.div_ceil(TILE_BYTES) as u32;
    }
    if let Some(pk) = packed.as_mut() {
        *pk = PackedMips {
            standard: standard as u8,
            packed: (mips - standard) as u8,
            tiles: u32::from(standard < mips),
            start: 0,
        };
    }
    if let Some(s) = shape.as_mut() {
        *s = [p.block[0] * side, p.block[1] * side, p.block[2]];
    }
    if let Some(n) = count.as_mut() {
        let total = mips * l.depth_or_array;
        let mut written = 0;
        for k in 0..*n {
            let sub = first + k;
            if sub >= total {
                break;
            }
            let (mip, slice) = ((sub % mips) as usize, sub / mips);
            if mip as u32 >= standard {
                continue;
            }
            written += 1;
            if let Some(out) = out.as_mut() {
                let m = &p.mips[mip];
                let start = p.subresource_offset(mip, slice) / TILE_BYTES;
                *(out as *mut SubresourceTiling).add(k as usize) = SubresourceTiling {
                    width: m.pitch / p.block[0],
                    height: (m.rows / p.block[1]) as u16,
                    depth: 1,
                    start: start as u32,
                };
            }
        }
        *n = written;
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(
        w: u64,
        h: u32,
        depth: u16,
        mips: u16,
        format: u32,
        layout: u32,
        flags: u32,
    ) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            width: w,
            height: h,
            depth_or_array_size: depth,
            mip_levels: mips,
            format,
            sample_count: 1,
            layout,
            flags,
            ..Default::default()
        }
    }

    fn make(d: &ResourceDesc) -> This {
        let mut out = std::ptr::null_mut();
        let hr = unsafe { XGCreateTextureComputer(d as *const _ as *const c_void, &mut out) };
        assert_eq!(hr, S_OK, "{d:?}");
        assert!(!out.is_null());
        out
    }

    /// The function in `slot` of the object's table, as the title would call it.
    unsafe fn method<F: Copy>(obj: This, slot: usize) -> F {
        let table = *(obj as *const *const *const ());
        std::mem::transmute_copy(&*table.add(slot))
    }

    fn suggested(d: &ResourceDesc) -> u32 {
        let mut out = 0;
        let hr = unsafe { XGSuggestSwizzleMode(d as *const _ as *const c_void, 0, &mut out) };
        assert_eq!(hr, S_OK);
        out
    }

    #[test]
    fn suggestions_follow_the_use() {
        assert_eq!(suggested(&desc(64, 64, 1, 1, 28, 0, 0)), 0x105);
        assert_eq!(suggested(&desc(64, 64, 1, 1, 28, 0, 1)), 0x11b);
        assert_eq!(suggested(&desc(64, 64, 1, 1, 40, 0, 2)), 0x118);
        assert_eq!(suggested(&desc(128, 128, 1, 1, 28, 0, 4)), 0x115);
        assert_eq!(suggested(&desc(256, 256, 1, 1, 28, 0, 4)), 0x11b);
        // console-only flags change nothing; a layout already in the description is ignored
        assert_eq!(suggested(&desc(64, 64, 1, 1, 28, 0x109, 0x30_0000)), 0x105);
        let mut line = desc(64, 1, 1, 1, 28, 0, 0);
        line.dimension = 2;
        assert_eq!(suggested(&line), 0x100);
        // what it suggests is usable
        let mut d = desc(100, 50, 1, 4, 28, 0, 0);
        d.layout = suggested(&d);
        make(&d);
    }

    #[test]
    fn null_and_bad_arguments() {
        let d = desc(64, 64, 1, 1, 28, 0, 0);
        let p = &d as *const _ as *const c_void;
        let mut out = 0u32;
        let mut obj = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                XGSuggestSwizzleMode(std::ptr::null(), 0, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                XGSuggestSwizzleMode(p, 0, std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(
                XGCreateTextureComputer(std::ptr::null(), &mut obj),
                E_INVALIDARG
            );
            assert_eq!(
                XGCreateTextureComputer(p, std::ptr::null_mut()),
                E_INVALIDARG
            );
            // variable-size modes and multisampling are not supported; nonsense is invalid
            for var in 28..=31 {
                let v = desc(64, 64, 1, 1, 28, 0x100 | var, 0);
                let hr = XGCreateTextureComputer(&v as *const _ as *const c_void, &mut obj);
                assert_eq!(hr, E_NOTIMPL);
                assert!(obj.is_null());
            }
            let mut msaa = d;
            msaa.sample_count = 4;
            assert_eq!(
                XGCreateTextureComputer(&msaa as *const _ as *const c_void, &mut obj),
                E_NOTIMPL
            );
            let zero = desc(0, 64, 1, 1, 28, 0, 0);
            assert_eq!(
                XGCreateTextureComputer(&zero as *const _ as *const c_void, &mut obj),
                E_INVALIDARG
            );
            let mut buffer = d;
            buffer.dimension = 1;
            assert_eq!(
                XGCreateTextureComputer(&buffer as *const _ as *const c_void, &mut obj),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn reference_layout_through_the_object() {
        let d = desc(3840, 2160, 1, 1, 28, 0, 1 | 4);
        let c = make(&d);
        unsafe {
            let size: unsafe extern "win64" fn(This) -> u64 = method(c, 3);
            let align: unsafe extern "win64" fn(This) -> u64 = method(c, 4);
            assert_eq!(size(c), 33_521_664);
            assert_eq!(align(c), 65_536);
            let get: unsafe extern "win64" fn(This, *mut LayoutOut) -> HRESULT = method(c, 2);
            let mut out: LayoutOut = std::mem::zeroed();
            assert_eq!(get(c, &mut out), S_OK);
            assert_eq!((out.size, out.alignment), (33_521_664, 65_536));
            assert_eq!((out.mips, out.planes, out.dimension), (1, 2, 3));
            let colour = &out.plane[0];
            assert_eq!(
                (colour.usage, colour.size, colour.offset),
                (1, 33_423_360, 0)
            );
            assert_eq!((colour.element_bytes, colour.alignment), (4, 65_536));
            assert_eq!((colour.first_tail_mip, colour.tail_mips), (1, 0));
            let m = &colour.mips[0];
            assert_eq!(
                (m.size, m.offset, m.pitch_texels, m.pitch_bytes),
                (33_423_360, 0, 3840, 15_360)
            );
            assert_eq!(
                (m.padded[0], m.padded[1], m.elements, m.texels),
                (3840, 2176, [3840, 2160], [3840, 2160])
            );
            assert_eq!(
                (m.swizzle, m.samples, m.in_tail, m.bank_rotation[0]),
                (27, 1, 0, 0x8000)
            );
            let cmask = &out.plane[1];
            assert_eq!(
                (cmask.usage, cmask.size, cmask.offset, cmask.alignment),
                (2, 98_304, 33_423_360, 0x2000)
            );
            assert_eq!(get(c, std::ptr::null_mut()), E_INVALIDARG);
            let release: unsafe extern "win64" fn(This) -> u32 = method(c, 1);
            assert_eq!(release(c), 0);
        }
    }

    #[test]
    fn numbers_equal_the_layout() {
        let d = desc(300, 200, 3, 0, 71, 0x100 | 27, 0);
        let l = Layout::new(&d).unwrap();
        let c = make(&d);
        unsafe {
            let add_ref: unsafe extern "win64" fn(This) -> u32 = method(c, 0);
            let release: unsafe extern "win64" fn(This) -> u32 = method(c, 1);
            assert_eq!(add_ref(c), 2);
            assert_eq!(release(c), 1);
            let mip_off: unsafe extern "win64" fn(This, u32, u32) -> u64 = method(c, 5);
            let texel: unsafe extern "win64" fn(
                This,
                u32,
                u32,
                u64,
                u32,
                u32,
                u32,
                *mut u32,
            ) -> u64 = method(c, 6);
            let mut bit = 7;
            for (i, m) in l.planes[0].mips.iter().enumerate() {
                assert_eq!(mip_off(c, 0, i as u32), m.offset);
                let a = Addresser::new(&l, 0, i, 0).unwrap();
                for slice in 0..3 {
                    let (x, y) = (m.width - 1, m.height - 1);
                    assert_eq!(
                        texel(c, 0, i as u32, x.into(), y, slice, 0, &mut bit),
                        a.offset(x, y, slice)
                    );
                    assert_eq!(bit, 0);
                }
                // outside: 0
                assert_eq!(
                    texel(
                        c,
                        0,
                        i as u32,
                        m.width.into(),
                        0,
                        0,
                        0,
                        std::ptr::null_mut()
                    ),
                    0
                );
            }
            assert_eq!(texel(c, 0, 0, 0, 0, 3, 0, std::ptr::null_mut()), 0);
            assert_eq!(texel(c, 1, 0, 0, 0, 0, 0, std::ptr::null_mut()), 0);
            assert_eq!(mip_off(c, 0, 99), 0);
            let get: unsafe extern "win64" fn(This, *mut LayoutOut) -> HRESULT = method(c, 2);
            let mut out: LayoutOut = std::mem::zeroed();
            get(c, &mut out);
            let p = &out.plane[0];
            assert_eq!(p.slice_size, l.planes[0].slice_size);
            let first = l.planes[0]
                .mips
                .iter()
                .position(|m| m.tail.is_some())
                .unwrap();
            assert!(first > 0);
            assert_eq!(p.first_tail_mip as usize, first);
            for (i, m) in l.planes[0].mips.iter().enumerate() {
                assert_eq!(p.mips[i].offset, m.offset);
                assert_eq!(p.mips[i].in_tail, u32::from(m.tail.is_some()));
            }
            // tail levels are in 256-byte blocks of the render kind
            assert_eq!((p.mips[0].swizzle, p.mips[8].swizzle), (27, 3));
            assert_eq!(
                (p.mips[0].pitch_texels, p.mips[0].texels),
                (4 * m_pitch(&l), [300, 200])
            );
            release(c);
        }
    }

    fn m_pitch(l: &Layout) -> u32 {
        l.planes[0].mips[0].pitch
    }

    #[test]
    fn copies_through_the_object() {
        let d = desc(70, 40, 2, 3, 28, 0x100 | 9, 0);
        let l = Layout::new(&d).unwrap();
        let c = make(&d);
        unsafe {
            let into: unsafe extern "win64" fn(
                This,
                *mut u8,
                u32,
                u32,
                *const u8,
                u32,
                u32,
            ) -> HRESULT = method(c, 8);
            let from: unsafe extern "win64" fn(
                This,
                *mut u8,
                u32,
                u32,
                *const u8,
                u32,
                u32,
            ) -> HRESULT = method(c, 9);
            let mut tiled = vec![0u8; l.size as usize];
            let src: Vec<u8> = (0..70 * 40 * 4).map(|i| (i * 7 % 251) as u8 | 1).collect();
            // subresource 3 = slice 1, mip 0
            assert_eq!(
                into(c, tiled.as_mut_ptr(), 0, 3, src.as_ptr(), 70 * 4, 0),
                S_OK
            );
            let mut back = vec![0u8; src.len()];
            assert_eq!(
                from(c, back.as_mut_ptr(), 0, 3, tiled.as_ptr(), 70 * 4, 0),
                S_OK
            );
            assert_eq!(back, src);
            // the other slice stayed empty
            assert_eq!(
                from(c, back.as_mut_ptr(), 0, 0, tiled.as_ptr(), 70 * 4, 0),
                S_OK
            );
            assert!(back.iter().all(|&b| b == 0));
            // errors
            assert_eq!(
                into(c, tiled.as_mut_ptr(), 0, 6, src.as_ptr(), 280, 0),
                E_INVALIDARG
            );
            assert_eq!(
                into(c, tiled.as_mut_ptr(), 1, 0, src.as_ptr(), 280, 0),
                E_INVALIDARG
            );
            assert_eq!(
                into(c, std::ptr::null_mut(), 0, 0, src.as_ptr(), 280, 0),
                E_INVALIDARG
            );
            assert_eq!(
                from(c, back.as_mut_ptr(), 0, 0, std::ptr::null(), 280, 0),
                E_INVALIDARG
            );
            assert_eq!(
                into(c, tiled.as_mut_ptr(), 0, 0, src.as_ptr(), 100, 0),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn resource_tiling() {
        // 64KB_S RGBA8 256 x 256 (blocks of 128 x 128), 9 levels, 2 slices: the tail starts at
        // level 2 (a level of at most 64 x 64 texels, at most 12 levels from the end)
        let d = desc(256, 256, 2, 9, 28, 0x100 | 9, 0);
        let l = Layout::new(&d).unwrap();
        let c = make(&d);
        unsafe {
            let tiling: unsafe extern "win64" fn(
                This,
                *mut u32,
                *mut PackedMips,
                *mut [u32; 3],
                *mut u32,
                u32,
                *mut SubresourceTiling,
            ) -> HRESULT = method(c, 10);
            let (mut tiles, mut shape, mut n) = (0u32, [0u32; 3], 18u32);
            let mut packed = PackedMips {
                standard: 0,
                packed: 0,
                tiles: 0,
                start: 9,
            };
            let mut out = [SubresourceTiling {
                width: 9,
                height: 9,
                depth: 9,
                start: 9,
            }; 18];
            assert_eq!(
                tiling(
                    c,
                    &mut tiles,
                    &mut packed,
                    &mut shape,
                    &mut n,
                    0,
                    out.as_mut_ptr()
                ),
                S_OK
            );
            let standard = l.planes[0]
                .mips
                .iter()
                .position(|m| m.tail.is_some())
                .unwrap();
            assert_eq!(tiles as u64, l.size / 65536);
            assert_eq!(
                (
                    packed.standard as usize,
                    packed.packed as usize,
                    packed.tiles,
                    packed.start
                ),
                (standard, 9 - standard, 1, 0)
            );
            assert_eq!(shape, [128, 128, 1]);
            assert_eq!(n as usize, standard * 2);
            // mip 0 of slice 0 is 2 x 2 tiles and starts after the tail tile
            assert_eq!(
                (out[0].width, out[0].height, out[0].depth, out[0].start),
                (2, 2, 1, 1 + (l.planes[0].mips[0].offset / 65536 - 1) as u32)
            );
            // slice 1 sits one slice further, packed levels are left untouched
            assert_eq!(
                out[9].start as u64,
                l.planes[0].subresource_offset(0, 1) / 65536
            );
            assert_eq!(out[standard].width, 9);
            // other modes: E_FAIL
            let small = make(&desc(64, 64, 1, 1, 28, 0, 0));
            assert_eq!(
                tiling(
                    small,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut()
                ),
                E_FAIL
            );
        }
    }
}
