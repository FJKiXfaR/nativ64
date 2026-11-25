//! The memory layout of a texture on the console: swizzle mode, blocks, mip chain with its
//! tail, planes and metadata, sizes and alignment. Pure computation, no I/O.
//!
//! The algorithms follow addrlib (`core/addrlib2.cpp`, `gfx10/gfx10addrlib.cpp`), written down
//! again for the one chip we need. See `docs/textures.md`.

use std::fmt;

use super::equation::{Config, Equation, EquationError, SwizzleMode};
use super::format::{format_info, FormatInfo, Planes};
use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::resource::ResourceDesc;
use crate::xobj::HRESULT;
use nativ64::com::E_NOTIMPL;

const TEXTURE_1D: u32 = 2;
const TEXTURE_3D: u32 = 4;
const FLAG_RENDER_TARGET: u32 = 0x1;
const FLAG_DEPTH_STENCIL: u32 = 0x2;
const FLAG_UAV: u32 = 0x4;
/// Largest width or height we lay out (twice what Direct3D 12 allows).
const MAX_DIM: u32 = 32768;
/// addrlib's `MaxMipLevels`.
const MAX_MIPS: u32 = 15;
const CMASK_ALIGNMENT: u64 = 0x2000;
const HTILE_ALIGNMENT: u64 = 0x10000;
const KIB_64: u64 = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Not a texture dimension (buffers and unknown).
    NotATexture(u32),
    /// A description that makes no sense (zero size, bad layout field or mip count).
    InvalidDesc(&'static str),
    UnknownFormat(u32),
    /// A swizzle mode we have no pattern for, or that the dimension cannot use.
    UnsupportedMode(u32),
    /// The variable-size modes (28-31).
    VarMode(u32),
    Msaa(u32),
    /// Elements that are not 1, 2, 4, 8 or 16 bytes.
    ElementSize(u32),
    TooLarge,
}

impl LayoutError {
    /// The result code the API reports.
    pub fn hresult(&self) -> HRESULT {
        match self {
            Self::NotATexture(_) | Self::InvalidDesc(_) | Self::TooLarge => E_INVALIDARG,
            _ => E_NOTIMPL,
        }
    }
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATexture(d) => write!(f, "dimension {d} is not a texture"),
            Self::InvalidDesc(why) => write!(f, "invalid description: {why}"),
            Self::UnknownFormat(x) => write!(f, "format {x} has no known layout"),
            Self::UnsupportedMode(m) => write!(f, "swizzle mode {m} is not supported here"),
            Self::VarMode(m) => write!(f, "variable-size swizzle mode {m} is not supported"),
            Self::Msaa(n) => write!(f, "{n} samples (multisampling is not supported)"),
            Self::ElementSize(n) => write!(f, "{n}-byte elements cannot be tiled"),
            Self::TooLarge => write!(f, "texture too large"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    /// Colour, or the depth plane of a depth format.
    Main,
    Stencil,
}

/// Where a mip level inside the tail block sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tail {
    /// Byte offset of the level inside the tail block (before it is multiplied by the depth
    /// of thick blocks; see `Mip::offset`).
    pub block_offset: u32,
    /// Texel position of the level inside the tail block's coordinate space, in elements.
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mip {
    /// Size in elements (not padded); depth is that of a 3D level, 1 otherwise.
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Padded width in elements (the row pitch is `pitch * element_bytes`) and padded rows.
    pub pitch: u32,
    pub rows: u32,
    /// Start of the level in slice 0 of the plane (for 3D, of the whole volume).
    pub offset: u64,
    /// `Some` for the levels packed in the tail block.
    pub tail: Option<Tail>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub kind: PlaneKind,
    /// From the start of the resource.
    pub offset: u64,
    pub size: u64,
    pub element_bytes: u32,
    /// Block size in elements (x, y, z); linear: (256 / element, 1, 1).
    pub block: [u32; 3],
    /// Bytes between two array slices (thin) or two depth slices (3D) of the plane: the whole
    /// mip chain of one slice.
    pub slice_size: u64,
    /// Slices stored: array size, or the depth of a 3D texture rounded up to its blocks.
    pub slices: u32,
    pub mips: Vec<Mip>,
}

impl Plane {
    /// Offset in the plane of mip `mip` of array slice `slice` (a 3D texture has slice 0 and
    /// finds its depth slices inside the mip).
    pub fn subresource_offset(&self, mip: usize, slice: u32) -> u64 {
        u64::from(slice) * self.slice_size + self.mips[mip].offset
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaKind {
    Cmask,
    Htile,
}

/// A metadata plane: reserved (it counts in the size), never read by us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: MetaKind,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub mode: SwizzleMode,
    pub dimension: u32,
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, array size otherwise.
    pub depth_or_array: u32,
    pub mip_levels: u32,
    /// Thick (3D) blocks.
    pub thick: bool,
    pub planes: Vec<Plane>,
    pub metadata: Vec<Metadata>,
    pub size: u64,
    pub alignment: u64,
}

fn log2(v: u32) -> u32 {
    v.trailing_zeros()
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

fn pow2_align(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

fn shift_ceil(v: u32, s: u32) -> u32 {
    (v.max(1) + ((1u32 << s) - 1)) >> s
}

/// addrlib's `Block256_2d` and `Block256_3d` by element size, in elements.
const BLOCK_256_2D: [[u32; 2]; 5] = [[16, 16], [16, 8], [8, 8], [8, 4], [4, 4]];
const BLOCK_256_3D: [[u32; 3]; 5] = [[8, 4, 8], [4, 4, 8], [4, 4, 4], [4, 2, 4], [2, 2, 4]];
/// addrlib's `Block1K_3d`.
const BLOCK_1K_3D: [[u32; 3]; 5] = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]];

/// Block size in elements (x, y, z) of `mode` for elements of `element_bytes` bytes. `thick`
/// is a 3D texture in a mode that has thick blocks.
pub fn block_dims(mode: SwizzleMode, thick: bool, element_bytes: u32) -> [u32; 3] {
    let elem_log2 = log2(element_bytes);
    if mode.is_linear() {
        return [256 / element_bytes, 1, 1];
    }
    let blk = mode.block_log2();
    if thick {
        let k = blk - 10;
        let (avg, rest) = (k / 3, k % 3);
        let b = BLOCK_1K_3D[elem_log2 as usize];
        [
            b[0] << avg,
            b[1] << (avg + rest / 2),
            b[2] << (avg + u32::from(rest != 0)),
        ]
    } else {
        let n = blk - elem_log2;
        let lw = n.div_ceil(2);
        [1 << lw, 1 << (n - lw), 1]
    }
}

/// addrlib's `GetMaxNumMipsInTail`.
fn max_mips_in_tail(block_log2: u32, thin: bool) -> u32 {
    let effective = if thin {
        block_log2
    } else {
        block_log2 - (block_log2 - 8) / 3
    };
    if effective <= 11 {
        1 + (1 << (effective - 9))
    } else {
        effective - 4
    }
}

struct Surface {
    block: [u32; 3],
    slice_size: u64,
    slices: u32,
    size: u64,
    mips: Vec<Mip>,
}

/// One plane: elements of `elem` bytes, `w` x `h` elements, `num_slices` array or depth
/// slices, `mips` levels.
fn surface(
    mode: SwizzleMode,
    thick: bool,
    elem: u32,
    (w, h): (u32, u32),
    num_slices: u32,
    mips: u32,
) -> Result<Surface, LayoutError> {
    let block = block_dims(mode, thick, elem);
    let [bw, bh, bd] = block;
    let e = u64::from(elem);
    let index = log2(elem) as usize;
    let mut out = Vec::with_capacity(mips as usize);
    let plain = |width: u32, height: u32, depth: u32, pitch: u32, rows: u32, offset: u64| Mip {
        width,
        height,
        depth,
        pitch,
        rows,
        offset,
        tail: None,
    };
    let mip0_depth = if thick { num_slices } else { 1 };
    let depth_of = |i: u32| if thick { shift_ceil(mip0_depth, i) } else { 1 };
    let slice_size;

    if mode.is_linear() || mode.block_log2() == 8 {
        // one block per 256 bytes (or a row): the mips follow each other, smallest first
        let (pitch_align, row_align) = (bw, bh);
        let mut total = 0u64;
        out.resize(mips as usize, plain(0, 0, 0, 0, 0, 0));
        for i in (0..mips).rev() {
            let (mw, mh) = (shift_ceil(w, i), shift_ceil(h, i));
            let (pitch, rows) = (pow2_align(mw, pitch_align), pow2_align(mh, row_align));
            out[i as usize] = plain(mw, mh, depth_of(i), pitch, rows, total);
            total += u64::from(pitch) * u64::from(rows) * e;
        }
        slice_size = total;
        let slices = num_slices;
        let size = slice_size
            .checked_mul(u64::from(slices))
            .ok_or(LayoutError::TooLarge)?;
        return Ok(Surface {
            block,
            slice_size,
            slices,
            size,
            mips: out,
        });
    }

    // macro tiled (4 KiB and 64 KiB blocks)
    let block_bytes = mode.block_bytes();
    let blk_log2 = mode.block_log2();
    let slices = pow2_align(num_slices, bd);
    if mips > 1 {
        let mut tail_dim = [bw, bh, bd];
        if thick {
            match blk_log2 % 3 {
                0 => tail_dim[1] >>= 1,
                1 => tail_dim[0] >>= 1,
                _ => tail_dim[2] >>= 1,
            }
        } else {
            tail_dim[0] >>= 1;
        }
        let mut fixed = tail_dim;
        if mode.is_z() && index <= 1 {
            fixed[0] /= BLOCK_256_2D[index][0] / BLOCK_256_2D[2][0];
            fixed[1] /= BLOCK_256_2D[index][1] / BLOCK_256_2D[2][1];
        }
        let max_tail = max_mips_in_tail(blk_log2, !thick);
        let mut first_tail = mips;
        let mut chain = 0u64;
        let mut mip_size = vec![0u64; mips as usize];
        for i in 0..mips {
            let (mw, mh, md) = (shift_ceil(w, i), shift_ceil(h, i), depth_of(i));
            if mw <= fixed[0] && mh <= fixed[1] && mips - i <= max_tail {
                first_tail = i;
                chain += block_bytes / u64::from(bd);
                break;
            }
            let (pitch, rows, depth) = (pow2_align(mw, bw), pow2_align(mh, bh), pow2_align(md, bd));
            let slice = u64::from(pitch) * u64::from(rows) * e;
            mip_size[i as usize] = slice * u64::from(depth);
            chain += slice;
            out.push(plain(mw, mh, md, pitch, rows, 0));
        }
        slice_size = chain;
        let mut offset = 0u64;
        let mut tail_depth = 0;
        if first_tail != mips {
            tail_depth = depth_of(first_tail);
            offset = block_bytes * u64::from(pow2_align(tail_depth, bd)) / u64::from(bd);
        }
        for i in (0..first_tail as usize).rev() {
            out[i].offset = offset;
            offset += mip_size[i];
        }
        let mut pitch = tail_dim[0];
        let mut rows = tail_dim[1];
        let depth = if thick {
            pow2_align(tail_depth, BLOCK_256_3D[index][2])
        } else {
            1
        };
        let tail_blocks = if thick {
            u64::from(depth / BLOCK_256_3D[index][2])
        } else {
            1
        };
        for i in first_tail..mips {
            let m = max_tail - 1 - (i - first_tail);
            let mip_offset = if m > 6 { 16u32 << m } else { m << 8 };
            let mut x = ((mip_offset >> 9) & 1)
                | ((mip_offset >> 10) & 2)
                | ((mip_offset >> 11) & 4)
                | ((mip_offset >> 12) & 8)
                | ((mip_offset >> 13) & 16)
                | ((mip_offset >> 14) & 32);
            let mut y = ((mip_offset >> 8) & 1)
                | ((mip_offset >> 9) & 2)
                | ((mip_offset >> 10) & 4)
                | ((mip_offset >> 11) & 8)
                | ((mip_offset >> 12) & 16)
                | ((mip_offset >> 13) & 32);
            if blk_log2 & 1 == 1 {
                std::mem::swap(&mut x, &mut y);
                if index & 1 == 1 {
                    y = (y << 1) | (x & 1);
                    x >>= 1;
                }
            }
            let b256 = if thick {
                [BLOCK_256_3D[index][0], BLOCK_256_3D[index][1]]
            } else {
                BLOCK_256_2D[index]
            };
            out.push(Mip {
                width: shift_ceil(w, i),
                height: shift_ceil(h, i),
                depth: depth_of(i),
                pitch,
                rows,
                offset: u64::from(mip_offset) * tail_blocks,
                tail: Some(Tail {
                    block_offset: mip_offset,
                    x: x * b256[0],
                    y: y * b256[1],
                }),
            });
            pitch = (pitch >> 1).max(b256[0]);
            rows = (rows >> 1).max(b256[1]);
        }
    } else {
        let (pitch, rows) = (pow2_align(w, bw), pow2_align(h, bh));
        slice_size = u64::from(pitch) * u64::from(rows) * e;
        out.push(plain(w, h, mip0_depth, pitch, rows, 0));
    }
    let size = slice_size
        .checked_mul(u64::from(slices))
        .ok_or(LayoutError::TooLarge)?;
    Ok(Surface {
        block,
        slice_size,
        slices,
        size,
        mips: out,
    })
}

/// The mode the library picks for a description whose layout field is 0.
fn choose_mode(dimension: u32, flags: u32, info: &FormatInfo, w: u32, h: u32) -> SwizzleMode {
    if dimension == TEXTURE_1D {
        SwizzleMode::LINEAR
    } else if flags & FLAG_DEPTH_STENCIL != 0 {
        SwizzleMode::SW_64KB_Z_X
    } else if flags & FLAG_RENDER_TARGET != 0 {
        SwizzleMode::SW_64KB_R_X
    } else if flags & FLAG_UAV != 0 {
        let bits = u64::from(info.element_bytes) * 8 * u64::from(w) * u64::from(h);
        if bits <= 0x80000 {
            SwizzleMode::SW_4KB_S_X
        } else {
            SwizzleMode::SW_64KB_R_X
        }
    } else {
        SwizzleMode::SW_4KB_S
    }
}

/// The mode the library suggests for `desc` (what `XGSuggestSwizzleMode` writes): the choice
/// for layout 0, whatever the description's layout field says. Unknown formats count as 32
/// bits per pixel.
pub fn suggest_mode(desc: &ResourceDesc) -> SwizzleMode {
    let info = format_info(desc.format).unwrap_or(FormatInfo {
        element_bytes: 4,
        block: (1, 1),
        planes: Planes::One,
        depth: false,
    });
    let width = u32::try_from(desc.width).unwrap_or(u32::MAX);
    choose_mode(desc.dimension, desc.flags, &info, width, desc.height)
}

impl Layout {
    /// The layout of the texture `desc` describes.
    pub fn new(desc: &ResourceDesc) -> Result<Layout, LayoutError> {
        let dimension = desc.dimension;
        if !(TEXTURE_1D..=TEXTURE_3D).contains(&dimension) {
            return Err(LayoutError::NotATexture(dimension));
        }
        if desc.sample_count > 1 {
            return Err(LayoutError::Msaa(desc.sample_count));
        }
        let info = format_info(desc.format).ok_or(LayoutError::UnknownFormat(desc.format))?;
        let (width, height) = (
            u32::try_from(desc.width).map_err(|_| LayoutError::TooLarge)?,
            desc.height,
        );
        let depth_or_array = u32::from(desc.depth_or_array_size);
        if width == 0 || height == 0 || depth_or_array == 0 {
            return Err(LayoutError::InvalidDesc("zero size"));
        }
        if width > MAX_DIM || height > MAX_DIM {
            return Err(LayoutError::TooLarge);
        }
        let is_3d = dimension == TEXTURE_3D;
        let biggest = if is_3d {
            width.max(height).max(depth_or_array)
        } else {
            width.max(height)
        };
        let mip_levels = match u32::from(desc.mip_levels) {
            0 => 32 - biggest.leading_zeros(),
            n => n,
        };
        if mip_levels > MAX_MIPS {
            return Err(LayoutError::InvalidDesc("mip count"));
        }
        if !info.element_bytes.is_power_of_two() {
            return Err(LayoutError::ElementSize(info.element_bytes));
        }

        let mode = match desc.layout {
            0 => choose_mode(dimension, desc.flags, &info, width, height),
            1 => SwizzleMode::LINEAR,
            l if l >> 8 == 1 => SwizzleMode(l & 0xff),
            _ => return Err(LayoutError::InvalidDesc("layout field")),
        };
        if mode.is_var() {
            return Err(LayoutError::VarMode(mode.0));
        }
        let usable = mode.is_supported()
            && match dimension {
                TEXTURE_1D => mode.allowed_1d(),
                TEXTURE_3D => mode.allowed_3d(),
                _ => true,
            };
        if !usable {
            return Err(LayoutError::UnsupportedMode(mode.0));
        }
        let thick = is_3d && mode.is_std_or_disp();

        let elements = (width.div_ceil(info.block.0), height.div_ceil(info.block.1));
        let mut planes = Vec::new();
        let mut end = 0u64;
        let mut alignment = if mode.is_linear() {
            256
        } else {
            mode.block_bytes()
        };
        let mut plane_specs = vec![(PlaneKind::Main, info.element_bytes)];
        if info.planes == Planes::DepthStencil {
            plane_specs.push((PlaneKind::Stencil, 1));
        }
        for (kind, elem) in plane_specs {
            let s = surface(mode, thick, elem, elements, depth_or_array, mip_levels)?;
            let offset = align_up(end, alignment);
            end = offset.checked_add(s.size).ok_or(LayoutError::TooLarge)?;
            planes.push(Plane {
                kind,
                offset,
                size: s.size,
                element_bytes: elem,
                block: s.block,
                slice_size: s.slice_size,
                slices: s.slices,
                mips: s.mips,
            });
        }

        let mut metadata = Vec::new();
        if !mode.is_linear() {
            let padded = u64::from(pow2_align(width, 1024)) * u64::from(pow2_align(height, 1024));
            let slices = u64::from(depth_or_array);
            let mut add = |kind, per_slice: u64, align: u64| {
                let offset = align_up(end, align);
                let size = per_slice * slices;
                end = offset + size;
                alignment = alignment.max(align.min(KIB_64));
                metadata.push(Metadata { kind, offset, size });
            };
            if desc.flags & FLAG_RENDER_TARGET != 0 && mip_levels == 1 {
                add(MetaKind::Cmask, padded / 128, CMASK_ALIGNMENT);
            }
            if desc.flags & FLAG_DEPTH_STENCIL != 0 && info.depth {
                add(MetaKind::Htile, padded / 16, HTILE_ALIGNMENT);
            }
        }

        if desc.alignment.is_power_of_two() {
            alignment = alignment.max(desc.alignment);
        }
        Ok(Layout {
            mode,
            dimension,
            width,
            height,
            depth_or_array,
            mip_levels,
            thick,
            planes,
            metadata,
            size: end,
            alignment,
        })
    }

    /// The address equation of plane `plane` (not defined for linear layouts).
    pub fn equation(&self, plane: usize, config: Config) -> Result<Equation, EquationError> {
        let p = &self.planes[plane];
        Equation::new(self.mode, self.thick, log2(p.element_bytes), config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tex(dimension: u32, w: u64, h: u32, depth: u16, mips: u16, format: u32) -> ResourceDesc {
        ResourceDesc {
            dimension,
            width: w,
            height: h,
            depth_or_array_size: depth,
            mip_levels: mips,
            format,
            sample_count: 1,
            ..Default::default()
        }
    }

    fn with(mut d: ResourceDesc, layout: u32, flags: u32) -> ResourceDesc {
        d.layout = layout;
        d.flags = flags;
        d
    }

    #[test]
    fn reference_render_target() {
        // 3840 x 2160 R8G8B8A8_UNORM, render target + UAV, 1 mip, library's choice
        let d = with(
            tex(3, 3840, 2160, 1, 1, 28),
            0,
            FLAG_RENDER_TARGET | FLAG_UAV,
        );
        let l = Layout::new(&d).unwrap();
        assert_eq!(l.mode, SwizzleMode::SW_64KB_R_X);
        let p = &l.planes[0];
        assert_eq!(p.block, [128, 128, 1]);
        assert_eq!(p.size, 30 * 17 * 65536);
        assert_eq!(p.size, 33_423_360);
        assert_eq!(
            l.metadata,
            vec![Metadata {
                kind: MetaKind::Cmask,
                offset: 33_423_360,
                size: 98_304
            }]
        );
        assert_eq!(l.size, 33_521_664);
        assert_eq!(l.alignment, 65536);
        // the console flags are ignored
        let d = with(d, 0, FLAG_RENDER_TARGET | FLAG_UAV | 0x10_0000 | 0x20_0000);
        assert_eq!(Layout::new(&d).unwrap().size, 33_521_664);
    }

    #[test]
    fn mode_choice() {
        let m = |dim, flags, w, h, fmt| {
            Layout::new(&with(tex(dim, w, h, 1, 1, fmt), 0, flags))
                .unwrap()
                .mode
        };
        assert_eq!(m(2, 0, 64, 1, 28), SwizzleMode::LINEAR);
        assert_eq!(
            m(3, FLAG_DEPTH_STENCIL, 64, 64, 40),
            SwizzleMode::SW_64KB_Z_X
        );
        assert_eq!(
            m(3, FLAG_RENDER_TARGET, 64, 64, 28),
            SwizzleMode::SW_64KB_R_X
        );
        // UAV only: 32 bits * 128 * 128 = 0x80000 fits, one more row does not
        assert_eq!(m(3, FLAG_UAV, 128, 128, 28), SwizzleMode::SW_4KB_S_X);
        assert_eq!(m(3, FLAG_UAV, 128, 129, 28), SwizzleMode::SW_64KB_R_X);
        assert_eq!(m(3, 0, 64, 64, 28), SwizzleMode::SW_4KB_S);
        // explicit layouts
        let explicit = |l| Layout::new(&with(tex(3, 64, 64, 1, 1, 28), l, 0)).map(|l| l.mode.0);
        assert_eq!(explicit(1), Ok(0));
        assert_eq!(explicit(0x100 | 9), Ok(9));
        assert_eq!(explicit(0x100 | 27), Ok(27));
    }

    #[test]
    fn linear_pitch_padding() {
        // 100 x 10 RGBA8 linear: pitch padded to 256 bytes = 64 elements -> 128
        let l = Layout::new(&with(tex(3, 100, 10, 1, 1, 28), 1, 0)).unwrap();
        let p = &l.planes[0];
        assert_eq!((p.mips[0].pitch, p.mips[0].rows), (128, 10));
        assert_eq!(p.size, 128 * 10 * 4);
        assert_eq!(l.alignment, 256);
        assert!(l.metadata.is_empty());
        // R8 linear, 3 mips: smallest first; mip 2 = 25 x 3 -> 256 x 3; mip 1 = 50 x 5; mip 0
        let l = Layout::new(&with(tex(3, 100, 10, 1, 3, 61), 1, 0)).unwrap();
        let o: Vec<u64> = l.planes[0].mips.iter().map(|m| m.offset).collect();
        assert_eq!(o, vec![256 * 3 + 256 * 5, 256 * 3, 0]);
        assert_eq!(l.planes[0].slice_size, 256 * 3 + 256 * 5 + 256 * 10);
    }

    #[test]
    fn small_4kb_standard() {
        // 4KB_S RGBA8: blocks of 32 x 32 (4096 / 4 = 1024 elements)
        let l = Layout::new(&with(tex(3, 40, 20, 1, 1, 28), 0, 0)).unwrap();
        assert_eq!(l.mode, SwizzleMode::SW_4KB_S);
        let p = &l.planes[0];
        assert_eq!(p.block, [32, 32, 1]);
        assert_eq!((p.mips[0].pitch, p.mips[0].rows), (64, 32));
        assert_eq!(l.size, 2 * 4096);
        assert_eq!(l.alignment, 4096);
    }

    #[test]
    fn bc_mip_chain_with_tail() {
        // BC1 (8-byte elements of 4 x 4 texels), 64KB_R_X: blocks of 128 x 64 elements.
        // 2048 x 2048 texels = 512 x 512 elements, 12 levels. The tail block takes the levels
        // up to 64 x 64 elements that are at most 12 from the end: from mip 3. Mips 2, 1, 0
        // are 128 x 128 (1 x 2 blocks), 256 x 256 (2 x 4), 512 x 512 (4 x 8).
        let l = Layout::new(&with(tex(3, 2048, 2048, 1, 12, 71), 0x100 | 27, 0)).unwrap();
        let p = &l.planes[0];
        assert_eq!(p.block, [128, 64, 1]);
        let tail: Vec<usize> = (0..12).filter(|&i| p.mips[i].tail.is_some()).collect();
        assert_eq!(tail, (3..12).collect::<Vec<_>>());
        assert_eq!(p.mips[2].offset, 65536);
        assert_eq!(p.mips[1].offset, 65536 + 2 * 65536);
        assert_eq!(p.mips[0].offset, 65536 + 2 * 65536 + 8 * 65536);
        assert_eq!(p.slice_size, 65536 * (1 + 2 + 8 + 32));
        // tail levels: m = 11 for mip 3 down to 3 for mip 11; offset 16 << m above 6, else m << 8
        let off = |i: usize| p.mips[i].tail.unwrap().block_offset;
        assert_eq!(
            (off(3), off(4), off(8), off(9), off(11)),
            (32768, 16384, 1536, 1280, 768)
        );
        assert_eq!(p.mips[3].offset, 32768);
        for w in p.mips[3..].windows(2) {
            assert!(off_of(&w[0]) > off_of(&w[1]));
        }
    }

    fn off_of(m: &Mip) -> u32 {
        m.tail.unwrap().block_offset
    }

    #[test]
    fn bc3_single_mip_and_odd_size() {
        // BC3 100 x 30 texels = 25 x 8 elements of 16 bytes; 4KB_S blocks of 16 x 16
        let l = Layout::new(&with(tex(3, 100, 30, 1, 1, 77), 0, 0)).unwrap();
        let p = &l.planes[0];
        assert_eq!((p.mips[0].width, p.mips[0].height), (25, 8));
        assert_eq!(p.block, [16, 16, 1]);
        assert_eq!((p.mips[0].pitch, p.mips[0].rows), (32, 16));
        assert_eq!(l.size, 2 * 4096);
    }

    #[test]
    fn depth_stencil_two_planes() {
        // D32_FLOAT_S8X24 depth buffer 1024 x 1024: Z_X, depth 128 x 128 blocks of 4 bytes,
        // stencil 256 x 256 blocks of 1 byte, then HTILE
        let d = with(tex(3, 1024, 1024, 1, 1, 20), 0, FLAG_DEPTH_STENCIL);
        let l = Layout::new(&d).unwrap();
        assert_eq!(l.mode, SwizzleMode::SW_64KB_Z_X);
        assert_eq!(l.planes.len(), 2);
        let (depth, stencil) = (&l.planes[0], &l.planes[1]);
        assert_eq!(
            (depth.kind, depth.element_bytes, depth.size),
            (PlaneKind::Main, 4, 64 * 65536)
        );
        assert_eq!(stencil.kind, PlaneKind::Stencil);
        assert_eq!(stencil.block, [256, 256, 1]);
        assert_eq!((stencil.offset, stencil.size), (64 * 65536, 16 * 65536));
        assert_eq!(l.metadata.len(), 1);
        assert_eq!(l.metadata[0].kind, MetaKind::Htile);
        assert_eq!(l.metadata[0].offset, 80 * 65536);
        assert_eq!(l.metadata[0].size, 1024 * 1024 / 16);
        assert_eq!(l.size, 80 * 65536 + 65536);
        // D16 depth alone: one plane
        let l = Layout::new(&with(tex(3, 256, 256, 1, 1, 55), 0, FLAG_DEPTH_STENCIL)).unwrap();
        assert_eq!(l.planes.len(), 1);
    }

    #[test]
    fn thick_3d_texture() {
        // 64KB_S 3D RGBA8: thick blocks of 32 x 32 x 16 elements (64 KiB / 4 = 16384)
        let l = Layout::new(&with(tex(4, 64, 32, 32, 1, 28), 0x100 | 9, 0)).unwrap();
        assert!(l.thick);
        let p = &l.planes[0];
        assert_eq!(p.block, [32, 32, 16]);
        assert_eq!(p.size, 65536 * 4);
        // thin 3D (Z_X / R_X stay thin): slices follow each other
        let l = Layout::new(&with(tex(4, 128, 128, 4, 1, 28), 0x100 | 27, 0)).unwrap();
        assert!(!l.thick);
        assert_eq!(l.planes[0].block, [128, 128, 1]);
        assert_eq!(l.planes[0].slice_size, 65536);
        assert_eq!(l.size, 4 * 65536);
    }

    #[test]
    fn array_slices() {
        let l = Layout::new(&with(tex(3, 256, 256, 3, 1, 28), 0x100 | 27, 0)).unwrap();
        let p = &l.planes[0];
        assert_eq!(p.slice_size, 4 * 65536);
        assert_eq!(p.slices, 3);
        assert_eq!(p.size, 3 * 4 * 65536);
        assert_eq!(p.subresource_offset(0, 2), 2 * 4 * 65536);
        // CMASK covers every slice: 1024 x 1024 / 128 = 8192 each
        let rt = with(tex(3, 256, 256, 3, 1, 28), 0x100 | 27, FLAG_RENDER_TARGET);
        let l = Layout::new(&rt).unwrap();
        assert_eq!(l.metadata[0].size, 3 * 8192);
    }

    #[test]
    fn full_mip_chain_and_micro_tiled() {
        // mip_levels 0 = the whole chain: 64 x 16 has 7 levels
        let l = Layout::new(&with(tex(3, 64, 16, 1, 0, 61), 0x100 | 1, 0)).unwrap();
        assert_eq!(l.mip_levels, 7);
        // 256B_S, 1-byte elements: blocks of 16 x 16; smallest level first, no tail
        let p = &l.planes[0];
        assert_eq!(p.block, [16, 16, 1]);
        assert!(p.mips.iter().all(|m| m.tail.is_none()));
        assert_eq!(p.mips[6].offset, 0);
        // levels 6..2 take one block (256 B) each, level 1 two, level 0 four
        assert_eq!(p.mips[0].offset, 7 * 256);
        assert_eq!(p.slice_size, 7 * 256 + 4 * 256);
    }

    #[test]
    fn errors() {
        let ok = tex(3, 64, 64, 1, 1, 28);
        assert_eq!(
            Layout::new(&tex(1, 64, 1, 1, 1, 0)),
            Err(LayoutError::NotATexture(1))
        );
        let mut msaa = ok;
        msaa.sample_count = 4;
        assert_eq!(Layout::new(&msaa), Err(LayoutError::Msaa(4)));
        assert_eq!(
            Layout::new(&tex(3, 64, 64, 1, 1, 116)),
            Err(LayoutError::UnknownFormat(116))
        );
        assert_eq!(
            Layout::new(&tex(3, 64, 64, 1, 1, 6)),
            Err(LayoutError::ElementSize(12))
        );
        for var in 28..=31 {
            assert_eq!(
                Layout::new(&with(ok, 0x100 | var, 0)),
                Err(LayoutError::VarMode(var))
            );
        }
        for bad in [3, 4, 23, 40] {
            assert_eq!(
                Layout::new(&with(ok, 0x100 | bad, 0)),
                Err(LayoutError::UnsupportedMode(bad))
            );
        }
        // 3D without a 3D mode, 1D with a 2D one
        assert_eq!(
            Layout::new(&with(tex(4, 8, 8, 8, 1, 28), 0x100 | 6, 0)),
            Err(LayoutError::UnsupportedMode(6))
        );
        assert_eq!(
            Layout::new(&with(tex(2, 8, 1, 1, 1, 28), 0x100 | 9, 0)),
            Err(LayoutError::UnsupportedMode(9))
        );
        assert!(matches!(
            Layout::new(&with(ok, 7, 0)),
            Err(LayoutError::InvalidDesc(_))
        ));
        assert!(matches!(
            Layout::new(&tex(3, 0, 64, 1, 1, 28)),
            Err(LayoutError::InvalidDesc(_))
        ));
        assert!(matches!(
            Layout::new(&tex(3, 64, 64, 1, 16, 28)),
            Err(LayoutError::InvalidDesc(_))
        ));
        assert_eq!(
            Layout::new(&tex(3, 40000, 64, 1, 1, 28)),
            Err(LayoutError::TooLarge)
        );
        assert_eq!(LayoutError::Msaa(2).hresult(), E_NOTIMPL);
        assert_eq!(LayoutError::TooLarge.hresult(), E_INVALIDARG);
    }

    #[test]
    fn subresources_do_not_overlap_and_stay_inside() {
        // every mip's padded blocks lie inside the plane; no two levels overlap
        for (mode, fmt, w, h, mips, arr) in [
            (24u32, 28u32, 700u32, 300u32, 10u16, 2u16),
            (27, 71, 1024, 512, 11, 1),
            (25, 61, 129, 77, 8, 3),
            (21, 28, 100, 100, 7, 1),
            (9, 10, 513, 513, 10, 2),
            (1, 28, 33, 17, 6, 1),
            (0, 28, 33, 17, 6, 1),
        ] {
            let layout = if mode == 0 { 1 } else { 0x100 | mode };
            let l = Layout::new(&with(tex(3, w.into(), h, arr, mips, fmt), layout, 0)).unwrap();
            for p in &l.planes {
                let mut spans = Vec::new();
                for m in &p.mips {
                    let bytes = if m.tail.is_some() {
                        0
                    } else {
                        u64::from(m.pitch) * u64::from(m.rows) * u64::from(p.element_bytes)
                    };
                    spans.push((m.offset, m.offset + bytes));
                    assert!(m.offset + bytes <= p.slice_size, "mode {mode}");
                }
                spans.sort();
                for pair in spans.windows(2) {
                    assert!(pair[0].1 <= pair[1].0, "mode {mode}: {pair:?}");
                }
                assert!(u64::from(p.slices) * p.slice_size <= p.size);
            }
        }
    }
}
