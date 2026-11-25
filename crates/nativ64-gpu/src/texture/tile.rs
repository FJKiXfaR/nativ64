//! Linear <-> console swizzled copies of one subresource (`tile` and `detile`), and the
//! address of one element (`Addresser`), which `xg_xs` answers texel queries with.
//!
//! Coordinates are in elements (texels, or 4 x 4 blocks of them for BC formats). A level is
//! padded to whole blocks; block `n` of a level starts `n * block_bytes` after the level, blocks
//! going along x, then y, then z (thick blocks). Inside a block the address equation of the mode
//! (`equation.rs`) gives the offset. The levels of the mip tail all live in the first block of
//! the slice, at a position (`Tail`) inside it.

use std::fmt;

use super::equation::{pipe_bank_xor, Config, Equation, EquationError, SERIES_X};
use super::layout::{Layout, Mip, Plane};

/// The configuration our resources are laid out for.
const CONSOLE: Config = SERIES_X;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileError {
    /// No such plane, mip level or array slice.
    NoSubresource,
    /// The tiled memory does not cover the plane.
    TiledTooSmall,
    /// The linear buffer does not hold the rows, or a pitch is smaller than a row.
    LinearTooSmall,
    Equation(EquationError),
}

impl fmt::Display for TileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSubresource => write!(f, "no such plane, mip level or slice"),
            Self::TiledTooSmall => write!(f, "tiled memory is smaller than the texture"),
            Self::LinearTooSmall => write!(f, "linear buffer or pitch too small"),
            Self::Equation(e) => write!(f, "no address equation: {e:?}"),
        }
    }
}

impl From<EquationError> for TileError {
    fn from(e: EquationError) -> Self {
        Self::Equation(e)
    }
}

/// Where the elements of one mip level of one plane lie, from the start of the resource.
pub struct Addresser<'a> {
    plane: &'a Plane,
    mip: &'a Mip,
    eq: Option<Equation>,
    xor: u32,
    thick: bool,
    block_bytes: u64,
    blocks_x: u64,
    blocks_y: u64,
}

impl<'a> Addresser<'a> {
    /// `base` is the address of the start of the resource; only the XOR modes look at it
    /// (through its low bits), which is zero for any resource placed at its alignment.
    pub fn new(layout: &'a Layout, plane: usize, mip: usize, base: u64) -> Result<Self, TileError> {
        let p = layout.planes.get(plane).ok_or(TileError::NoSubresource)?;
        let m = p.mips.get(mip).ok_or(TileError::NoSubresource)?;
        let eq = if layout.mode.is_linear() {
            None
        } else {
            Some(layout.equation(plane, CONSOLE)?)
        };
        Ok(Addresser {
            plane: p,
            mip: m,
            xor: pipe_bank_xor(base.wrapping_add(p.offset), layout.mode, CONSOLE),
            eq,
            thick: layout.thick,
            block_bytes: layout.mode.block_bytes(),
            blocks_x: u64::from(m.pitch / p.block[0]),
            blocks_y: u64::from(m.rows / p.block[1]),
        })
    }

    /// Byte offset from the start of the resource of element (`x`, `y`) of array slice or depth
    /// slice `layer`. The coordinates must lie inside the level (not checked).
    pub fn offset(&self, x: u32, y: u32, layer: u32) -> u64 {
        let (p, m) = (self.plane, self.mip);
        let e = u64::from(p.element_bytes);
        let slice = if self.thick { 0 } else { u64::from(layer) };
        let start = p.offset + slice * p.slice_size;
        let Some(eq) = &self.eq else {
            return start + m.offset + (u64::from(y) * u64::from(m.pitch) + u64::from(x)) * e;
        };
        let [bw, bh, bd] = p.block;
        let (inside, block) = match m.tail {
            // the tail levels sit in the first block of the slice (of each depth block)
            Some(t) => (
                (x + t.x, y + t.y),
                if self.thick { u64::from(layer / bd) } else { 0 },
            ),
            None => {
                let zb = if self.thick { u64::from(layer / bd) } else { 0 };
                let (xb, yb) = (u64::from(x / bw), u64::from(y / bh));
                ((x, y), (zb * self.blocks_y + yb) * self.blocks_x + xb)
            }
        };
        let level = if m.tail.is_some() { 0 } else { m.offset };
        let within = eq.offset(self.xor, inside.0, inside.1, layer);
        start + level + block * self.block_bytes + u64::from(within)
    }
}

/// The part of a resource a subresource covers, and what to do per element.
struct Walk<'a> {
    layout: &'a Layout,
    plane: usize,
    mip: usize,
    layer0: u32,
    /// Depth slices copied (1 unless 3D).
    depth: u32,
}

fn walk(layout: &Layout, plane: usize, subresource: u32) -> Result<Walk<'_>, TileError> {
    let p = layout.planes.get(plane).ok_or(TileError::NoSubresource)?;
    let (mip, slice) = (
        (subresource % layout.mip_levels) as usize,
        subresource / layout.mip_levels,
    );
    let m = p.mips.get(mip).ok_or(TileError::NoSubresource)?;
    if layout.dimension == 4 {
        if slice != 0 {
            return Err(TileError::NoSubresource);
        }
        // thin 3D levels keep every depth slice in storage; the texture's depth halves
        let depth = if layout.thick {
            m.depth
        } else {
            (layout.depth_or_array >> mip).max(1)
        };
        Ok(Walk {
            layout,
            plane,
            mip,
            layer0: 0,
            depth,
        })
    } else if slice >= layout.depth_or_array {
        Err(TileError::NoSubresource)
    } else {
        Ok(Walk {
            layout,
            plane,
            mip,
            layer0: slice,
            depth: 1,
        })
    }
}

/// Checks the linear buffer for `rows` rows of `row_bytes` and returns the byte position of
/// row `y` of depth slice `z`.
fn linear_extent(
    w: &Walk,
    len: usize,
    row_pitch: usize,
    slice_pitch: usize,
) -> Result<impl Fn(u32, u32) -> usize, TileError> {
    let m = &w.layout.planes[w.plane].mips[w.mip];
    let row_bytes = m.width as usize * w.layout.planes[w.plane].element_bytes as usize;
    let rows = m.height as usize;
    if (rows > 1 && row_pitch < row_bytes) || (w.depth > 1 && slice_pitch < row_pitch * rows) {
        return Err(TileError::LinearTooSmall);
    }
    let last = (w.depth as usize - 1) * slice_pitch + (rows - 1) * row_pitch + row_bytes;
    if last > len {
        return Err(TileError::LinearTooSmall);
    }
    Ok(move |y, z| z as usize * slice_pitch + y as usize * row_pitch)
}

/// Copies subresource `subresource` (mip + slice * mips) of plane `plane` out of the tiled
/// memory `tiled` (the whole resource, starting at address `base`) into `linear`: rows of
/// `row_pitch` bytes, depth slices of `slice_pitch` bytes (`pitch` is the pair). Only the texels are written.
pub fn detile(
    layout: &Layout,
    plane: usize,
    subresource: u32,
    base: u64,
    tiled: &[u8],
    linear: &mut [u8],
    (row_pitch, slice_pitch): (usize, usize),
) -> Result<(), TileError> {
    copy(
        layout,
        plane,
        subresource,
        base,
        tiled.len(),
        linear.len(),
        row_pitch,
        slice_pitch,
        |at, to, e| linear[to..to + e].copy_from_slice(&tiled[at..at + e]),
    )
}

/// The reverse of [`detile`]: writes the rows of `linear` into the tiled memory. Padding
/// between the texels is left as it was.
pub fn tile(
    layout: &Layout,
    plane: usize,
    subresource: u32,
    base: u64,
    tiled: &mut [u8],
    linear: &[u8],
    (row_pitch, slice_pitch): (usize, usize),
) -> Result<(), TileError> {
    copy(
        layout,
        plane,
        subresource,
        base,
        tiled.len(),
        linear.len(),
        row_pitch,
        slice_pitch,
        |at, from, e| tiled[at..at + e].copy_from_slice(&linear[from..from + e]),
    )
}

/// The loop of both directions: `each(tiled position, linear position, element bytes)`.
#[allow(clippy::too_many_arguments)]
fn copy(
    layout: &Layout,
    plane: usize,
    subresource: u32,
    base: u64,
    tiled_len: usize,
    linear_len: usize,
    row_pitch: usize,
    slice_pitch: usize,
    mut each: impl FnMut(usize, usize, usize),
) -> Result<(), TileError> {
    let w = walk(layout, plane, subresource)?;
    let p = &layout.planes[plane];
    if (tiled_len as u64) < p.offset + p.size {
        return Err(TileError::TiledTooSmall);
    }
    let row_start = linear_extent(&w, linear_len, row_pitch, slice_pitch)?;
    let addr = Addresser::new(layout, plane, w.mip, base)?;
    let m = &p.mips[w.mip];
    let e = p.element_bytes as usize;
    for z in 0..w.depth {
        for y in 0..m.height {
            let row = row_start(y, z);
            for x in 0..m.width {
                let at = addr.offset(x, y, w.layer0 + z) as usize;
                each(at, row + x as usize * e, e);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::resource::ResourceDesc;

    #[allow(clippy::too_many_arguments)]
    fn desc(
        dimension: u32,
        (w, h, depth): (u64, u32, u16),
        mips: u16,
        format: u32,
        layout: u32,
        flags: u32,
    ) -> ResourceDesc {
        ResourceDesc {
            dimension,
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

    /// A deterministic, non-repeating-looking byte pattern per (seed, index).
    fn pattern(seed: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                let v = (i as u32).wrapping_mul(2_654_435_761) ^ seed.wrapping_mul(40_503);
                (v >> 13) as u8 | 1
            })
            .collect()
    }

    /// Tiles every subresource of every plane into one buffer, then detiles each and compares:
    /// catches wrong addresses, overlaps between texels and between subresources.
    fn round_trip(d: &ResourceDesc) {
        let l = Layout::new(d).unwrap_or_else(|e| panic!("{d:?}: {e}"));
        let mut tiled = vec![0u8; l.size as usize];
        let subs = l.mip_levels
            * if l.dimension == 4 {
                1
            } else {
                l.depth_or_array
            };
        let mut inputs = Vec::new();
        for (pi, p) in l.planes.iter().enumerate() {
            for sub in 0..subs {
                let m = &p.mips[(sub % l.mip_levels) as usize];
                let depth = if l.dimension != 4 {
                    1
                } else if l.thick {
                    m.depth
                } else {
                    (l.depth_or_array >> (sub % l.mip_levels)).max(1)
                };
                // pitches larger than the rows, to see that padding is respected
                let row = m.width as usize * p.element_bytes as usize + 5;
                let slice = row * m.height as usize + 3;
                let len = slice * depth as usize;
                let src = pattern(sub * 7 + pi as u32, len);
                tile(&l, pi, sub, 0, &mut tiled, &src, (row, slice))
                    .unwrap_or_else(|e| panic!("{d:?} tile {pi}/{sub}: {e}"));
                inputs.push((pi, sub, row, slice, depth, src));
            }
        }
        for (pi, sub, row, slice, depth, src) in inputs {
            let p = &l.planes[pi];
            let m = &p.mips[(sub % l.mip_levels) as usize];
            let mut out = vec![0u8; src.len()];
            detile(&l, pi, sub, 0, &tiled, &mut out, (row, slice)).unwrap();
            let bytes = m.width as usize * p.element_bytes as usize;
            for z in 0..depth as usize {
                for y in 0..m.height as usize {
                    let at = z * slice + y * row;
                    assert_eq!(
                        out[at..at + bytes],
                        src[at..at + bytes],
                        "{d:?}: plane {pi} subresource {sub} slice {z} row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn round_trip_linear_and_micro_tiled() {
        round_trip(&desc(3, (100, 10, 1), 3, 28, 1, 0));
        round_trip(&desc(3, (33, 17, 2), 6, 61, 1, 0));
        round_trip(&desc(3, (64, 16, 1), 0, 61, 0x100 | 1, 0));
        round_trip(&desc(3, (70, 20, 1), 0, 28, 0x100 | 2, 0));
        round_trip(&desc(2, (300, 1, 1), 1, 28, 0, 0));
    }

    #[test]
    fn round_trip_every_macro_mode() {
        for mode in [5, 6, 9, 10, 17, 18, 21, 22, 24, 25, 26, 27] {
            for (fmt, w, h, mips, arr) in [
                (61, 100, 37, 7, 1),  // 1-byte elements
                (54, 70, 90, 0, 2),   // 2
                (28, 513, 257, 0, 2), // 4, a full chain and an array
                (10, 129, 33, 5, 1),  // 8
                (2, 64, 100, 4, 3),   // 16
                (71, 300, 200, 0, 1), // BC1
                (77, 130, 70, 6, 2),  // BC3
            ] {
                round_trip(&desc(3, (w, h, arr), mips, fmt, 0x100 | mode, 0));
            }
        }
    }

    #[test]
    fn round_trip_depth_stencil_and_metadata() {
        // two planes (depth, stencil) and an HTILE reserved after them
        round_trip(&desc(3, (200, 100, 1), 1, 20, 0, 2));
        round_trip(&desc(3, (64, 64, 1), 5, 45, 0x100 | 24, 0));
        // a render target with its CMASK
        round_trip(&desc(3, (256, 128, 1), 1, 28, 0, 1));
    }

    #[test]
    fn round_trip_volumes() {
        // thick blocks (S modes), with a mip tail
        round_trip(&desc(4, (40, 20, 10), 3, 28, 0x100 | 9, 0));
        round_trip(&desc(4, (33, 17, 9), 0, 61, 0x100 | 5, 0));
        round_trip(&desc(4, (16, 16, 16), 5, 54, 0x100 | 21, 0));
        // thin blocks (Z_X, R_X)
        round_trip(&desc(4, (64, 64, 5), 3, 28, 0x100 | 27, 0));
        round_trip(&desc(4, (30, 20, 3), 1, 28, 0x100 | 24, 0));
    }

    #[test]
    fn hand_checked_addresses() {
        // linear RGBA8 100 x 10: rows 128 elements apart
        let l = Layout::new(&desc(3, (100, 10, 1), 1, 28, 1, 0)).unwrap();
        let a = Addresser::new(&l, 0, 0, 0).unwrap();
        assert_eq!(a.offset(0, 0, 0), 0);
        assert_eq!(a.offset(3, 2, 0), (2 * 128 + 3) * 4);
        // 4KB_S RGBA8 40 x 20: blocks of 32 x 32 along x
        let l = Layout::new(&desc(3, (40, 20, 1), 1, 28, 0, 0)).unwrap();
        let a = Addresser::new(&l, 0, 0, 0).unwrap();
        assert_eq!(a.offset(0, 0, 0), 0);
        assert!((4096..8192).contains(&a.offset(33, 0, 0)));
        assert!(a.offset(31, 31, 0) < 4096);
        // an array slice is a slice size further
        let l = Layout::new(&desc(3, (40, 20, 3), 1, 28, 0, 0)).unwrap();
        let a = Addresser::new(&l, 0, 0, 0).unwrap();
        assert_eq!(
            a.offset(5, 6, 2) - a.offset(5, 6, 0),
            2 * l.planes[0].slice_size
        );
    }

    #[test]
    fn tail_levels_share_the_first_block() {
        let l = Layout::new(&desc(3, (256, 256, 1), 0, 28, 0x100 | 27, 0)).unwrap();
        let p = &l.planes[0];
        let first = p.mips.iter().position(|m| m.tail.is_some()).unwrap();
        assert!(first > 0 && first < p.mips.len());
        for i in 0..p.mips.len() {
            let a = Addresser::new(&l, 0, i, 0).unwrap();
            let at = a.offset(0, 0, 0);
            if i >= first {
                assert!(at < 65536, "mip {i}");
            } else {
                assert!(at >= 65536, "mip {i}");
            }
        }
    }

    #[test]
    fn errors() {
        let l = Layout::new(&desc(3, (64, 64, 2), 3, 28, 0, 0)).unwrap();
        let mut tiled = vec![0u8; l.size as usize];
        let mut lin = vec![0u8; 64 * 64 * 4];
        let ok = |sub| detile(&l, 0, sub, 0, &tiled, &mut lin.clone(), (256, 0));
        assert_eq!(ok(0), Ok(()));
        assert_eq!(ok(5), Ok(()));
        // 3 mips x 2 slices
        assert_eq!(ok(6), Err(TileError::NoSubresource));
        assert_eq!(
            detile(&l, 1, 0, 0, &tiled, &mut lin, (256, 0)),
            Err(TileError::NoSubresource)
        );
        // pitch smaller than a row, buffer smaller than the rows
        assert_eq!(
            detile(&l, 0, 0, 0, &tiled, &mut lin, (255, 0)),
            Err(TileError::LinearTooSmall)
        );
        let mut small = vec![0u8; 64 * 63 * 4];
        assert_eq!(
            detile(&l, 0, 0, 0, &tiled, &mut small, (256, 0)),
            Err(TileError::LinearTooSmall)
        );
        // tiled memory shorter than the plane
        let short = &tiled[..tiled.len() - 1];
        assert_eq!(
            detile(&l, 0, 0, 0, short, &mut lin, (256, 0)),
            Err(TileError::TiledTooSmall)
        );
        let linear = vec![0u8; 64 * 64 * 4];
        let n = tiled.len() - 1;
        assert_eq!(
            tile(&l, 0, 0, 0, &mut tiled[..n], &linear, (256, 0)),
            Err(TileError::TiledTooSmall)
        );
        // nothing outside the texels is written: a 1-row, 1-element level only touches 4 bytes
        let l = Layout::new(&desc(3, (1, 1, 1), 1, 28, 1, 0)).unwrap();
        let mut t = vec![0u8; l.size as usize];
        tile(&l, 0, 0, 0, &mut t, &[9, 8, 7, 6], (4, 0)).unwrap();
        assert_eq!(t.iter().filter(|&&b| b != 0).count(), 4);
    }
}
