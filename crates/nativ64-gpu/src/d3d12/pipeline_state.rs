//! The fixed-function state of a graphics pipeline: blend, sample mask, rasterizer, depth and
//! stencil, strip cut. `GraphicsState::raw` keeps the description or the subobject stream the
//! title passed; this reads the two forms into one plain struct a backend maps to its own
//! pipeline (the formats, topology type, samples and input layout are read at creation).
//!
//! The description form is read at the offsets of the standard `D3D12_GRAPHICS_PIPELINE_STATE_DESC`
//! (blend at 120, sample mask 448, rasterizer 452, depth-stencil 496, strip cut 568); the
//! stream form by walking its subobjects, each state at +4 of its type word (the console's
//! `DEPTH_STENCIL1` adds the bounds test flag). An absent subobject has D3D12's default.
//!
//! shortcut: **[unverified]** `pipeline.rs` reads the input layout and formats of the same
//! description at offsets 8 bytes above these (it counts the stream output as 40 bytes, the
//! header's is 32); no title has been seen creating a pipeline from the description form, only
//! from streams, so the difference was never exercised.

use super::pipeline::{subobject, GraphicsState};

/// One render target's blend state (`D3D12_RENDER_TARGET_BLEND_DESC`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blend {
    pub enable: bool,
    pub logic_enable: bool,
    pub src: u32,
    pub dst: u32,
    pub op: u32,
    pub src_alpha: u32,
    pub dst_alpha: u32,
    pub op_alpha: u32,
    pub logic_op: u32,
    pub write_mask: u8,
}

impl Default for Blend {
    fn default() -> Self {
        Blend {
            enable: false,
            logic_enable: false,
            src: 2, // ONE
            dst: 1, // ZERO
            op: 1,  // ADD
            src_alpha: 2,
            dst_alpha: 1,
            op_alpha: 1,
            logic_op: 4, // NOOP
            write_mask: 0xf,
        }
    }
}

/// `D3D12_DEPTH_STENCILOP_DESC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StencilFace {
    pub fail: u32,
    pub depth_fail: u32,
    pub pass: u32,
    pub func: u32,
}

impl Default for StencilFace {
    fn default() -> Self {
        StencilFace {
            fail: 1,
            depth_fail: 1,
            pass: 1,
            func: 8, // ALWAYS
        }
    }
}

/// `D3D12_DEPTH_STENCIL_DESC` (and the bounds test flag of the console's second form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepthStencil {
    pub depth_enable: bool,
    /// Write mask ALL.
    pub depth_write: bool,
    pub depth_func: u32,
    pub stencil_enable: bool,
    pub read_mask: u8,
    pub write_mask: u8,
    pub front: StencilFace,
    pub back: StencilFace,
    pub bounds_enable: bool,
}

impl Default for DepthStencil {
    fn default() -> Self {
        DepthStencil {
            depth_enable: true,
            depth_write: true,
            depth_func: 2, // LESS
            stencil_enable: false,
            read_mask: 0xff,
            write_mask: 0xff,
            front: StencilFace::default(),
            back: StencilFace::default(),
            bounds_enable: false,
        }
    }
}

/// The fixed state, in D3D12's enumerations (a backend translates them).
#[derive(Clone, Debug, PartialEq)]
pub struct FixedState {
    pub alpha_to_coverage: bool,
    pub independent_blend: bool,
    pub targets: [Blend; 8],
    pub sample_mask: u32,
    /// 2 wireframe, 3 solid.
    pub fill: u32,
    /// 1 none, 2 front, 3 back.
    pub cull: u32,
    pub front_ccw: bool,
    pub depth_bias: i32,
    pub depth_bias_clamp: f32,
    pub slope_bias: f32,
    pub depth_clip: bool,
    /// `ConservativeRaster` is on.
    pub conservative: bool,
    /// 0 disabled, 1 0xffff, 2 0xffffffff.
    pub strip_cut: u32,
    pub depth: DepthStencil,
}

impl Default for FixedState {
    fn default() -> Self {
        FixedState {
            alpha_to_coverage: false,
            independent_blend: false,
            targets: [Blend::default(); 8],
            sample_mask: u32::MAX,
            fill: 3,
            cull: 3,
            front_ccw: false,
            depth_bias: 0,
            depth_bias_clamp: 0.0,
            slope_bias: 0.0,
            depth_clip: true,
            conservative: false,
            strip_cut: 0,
            depth: DepthStencil::default(),
        }
    }
}

/// The `n`-th dword at `at` (zero past the end, so a short stream reads defaults of zero:
/// callers only read inside what they checked).
fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
}

fn blend_at(b: &[u8], at: usize) -> Blend {
    Blend {
        enable: u32_at(b, at) != 0,
        logic_enable: u32_at(b, at + 4) != 0,
        src: u32_at(b, at + 8),
        dst: u32_at(b, at + 12),
        op: u32_at(b, at + 16),
        src_alpha: u32_at(b, at + 20),
        dst_alpha: u32_at(b, at + 24),
        op_alpha: u32_at(b, at + 28),
        logic_op: u32_at(b, at + 32),
        write_mask: b.get(at + 36).copied().unwrap_or(0xf),
    }
}

fn face_at(b: &[u8], at: usize) -> StencilFace {
    StencilFace {
        fail: u32_at(b, at),
        depth_fail: u32_at(b, at + 4),
        pass: u32_at(b, at + 8),
        func: u32_at(b, at + 12),
    }
}

impl FixedState {
    fn blend(&mut self, b: &[u8], at: usize) {
        self.alpha_to_coverage = u32_at(b, at) != 0;
        self.independent_blend = u32_at(b, at + 4) != 0;
        for (i, t) in self.targets.iter_mut().enumerate() {
            *t = blend_at(b, at + 8 + 40 * i);
        }
    }

    fn raster(&mut self, b: &[u8], at: usize) {
        self.fill = u32_at(b, at);
        self.cull = u32_at(b, at + 4);
        self.front_ccw = u32_at(b, at + 8) != 0;
        self.depth_bias = u32_at(b, at + 12) as i32;
        self.depth_bias_clamp = f32::from_bits(u32_at(b, at + 16));
        self.slope_bias = f32::from_bits(u32_at(b, at + 20));
        self.depth_clip = u32_at(b, at + 24) != 0;
        self.conservative = u32_at(b, at + 40) != 0;
    }

    fn depth_stencil(&mut self, b: &[u8], at: usize) {
        let bounds = self.depth.bounds_enable;
        self.depth = DepthStencil {
            depth_enable: u32_at(b, at) != 0,
            depth_write: u32_at(b, at + 4) == 1,
            depth_func: u32_at(b, at + 8),
            stencil_enable: u32_at(b, at + 12) != 0,
            read_mask: b.get(at + 16).copied().unwrap_or(0xff),
            write_mask: b.get(at + 17).copied().unwrap_or(0xff),
            front: face_at(b, at + 20),
            back: face_at(b, at + 36),
            bounds_enable: bounds,
        };
    }
}

/// Offsets in the description form.
mod desc {
    pub const BLEND: usize = 120;
    pub const SAMPLE_MASK: usize = 448;
    pub const RASTERIZER: usize = 452;
    pub const DEPTH_STENCIL: usize = 496;
    pub const STRIP_CUT: usize = 568;
    /// Bytes the reads above need.
    pub const NEEDED: usize = 572;
}

/// The fixed state of `g`, or why it cannot be read (a description or stream too short).
pub fn fixed_state(g: &GraphicsState) -> Result<FixedState, String> {
    let b = &g.raw[..];
    let mut s = FixedState::default();
    if !g.stream {
        if b.len() < desc::NEEDED {
            return Err(format!("a pipeline description of {} bytes", b.len()));
        }
        s.blend(b, desc::BLEND);
        s.sample_mask = u32_at(b, desc::SAMPLE_MASK);
        s.raster(b, desc::RASTERIZER);
        s.depth_stencil(b, desc::DEPTH_STENCIL);
        s.strip_cut = u32_at(b, desc::STRIP_CUT);
        return Ok(s);
    }
    let mut at = 0;
    while at + 4 <= b.len() {
        let kind = u32_at(b, at);
        let Some((len, _)) = subobject(kind) else {
            return Err(format!("stream subobject type {kind}"));
        };
        if at + len > b.len() {
            return Err(format!("stream subobject {kind} past the end"));
        }
        match kind {
            8 => s.blend(b, at + 4),
            9 => s.sample_mask = u32_at(b, at + 4),
            10 => s.raster(b, at + 4),
            11 => s.depth_stencil(b, at + 4),
            21 => {
                s.depth_stencil(b, at + 4);
                s.depth.bounds_enable = u32_at(b, at + 4 + 52) != 0;
            }
            13 => s.strip_cut = u32_at(b, at + 4),
            _ => {}
        }
        at += len;
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn state(raw: Vec<u8>, stream: bool) -> GraphicsState {
        GraphicsState {
            stream,
            raw,
            ..Default::default()
        }
    }

    #[test]
    fn the_description_form_is_read_at_the_header_offsets() {
        let mut b = vec![0u8; 0x298];
        put(&mut b, 120, 1); // alpha to coverage
        put(&mut b, 124, 1); // independent
        put(&mut b, 128 + 40 * 2, 1); // target 2 blend enable
        put(&mut b, 128 + 40 * 2 + 8, 5); // src alpha
        b[128 + 40 * 2 + 36] = 0x5;
        put(&mut b, 448, 0xff);
        put(&mut b, 452, 2); // fill wireframe
        put(&mut b, 456, 1); // cull none
        put(&mut b, 460, 1); // front ccw
        put(&mut b, 464, (-3i32) as u32);
        put(&mut b, 468, 0.5f32.to_bits());
        put(&mut b, 472, 2.0f32.to_bits());
        put(&mut b, 476, 0); // depth clip off
        put(&mut b, 492, 1); // conservative
        put(&mut b, 496, 1);
        put(&mut b, 500, 1);
        put(&mut b, 504, 4); // depth func
        put(&mut b, 508, 1);
        b[512] = 0x0f;
        b[513] = 0xf0;
        put(&mut b, 516 + 8, 3); // front pass op
        put(&mut b, 516 + 16 + 12, 5); // back func
        put(&mut b, 568, 2);
        let s = fixed_state(&state(b, false)).unwrap();
        assert!(s.alpha_to_coverage && s.independent_blend);
        let t = s.targets[2];
        assert!(t.enable && t.src == 5 && t.write_mask == 5);
        assert!(!s.targets[0].enable);
        assert_eq!(
            (s.sample_mask, s.fill, s.cull, s.front_ccw),
            (0xff, 2, 1, true)
        );
        assert_eq!(
            (s.depth_bias, s.depth_bias_clamp, s.slope_bias),
            (-3, 0.5, 2.0)
        );
        assert!(!s.depth_clip && s.conservative);
        let d = s.depth;
        assert!(d.depth_enable && d.depth_write && d.stencil_enable);
        assert_eq!((d.depth_func, d.read_mask, d.write_mask), (4, 0x0f, 0xf0));
        assert_eq!((d.front.pass, d.back.func), (3, 5));
        assert_eq!(s.strip_cut, 2);
    }

    #[test]
    fn the_stream_form_walks_its_subobjects_and_defaults_the_rest() {
        // blend (336), sample mask (8), rasterizer (48), depth-stencil 1 (64), strip cut (8)
        let mut b = vec![0u8; 336 + 8 + 48 + 64 + 8];
        put(&mut b, 0, 8);
        put(&mut b, 4, 1);
        put(&mut b, 12, 1); // target 0 enabled
        put(&mut b, 12 + 16, 1); // its blend op
        put(&mut b, 336, 9);
        put(&mut b, 340, 0xf0f0);
        put(&mut b, 344, 10);
        put(&mut b, 348 + 4, 1); // cull none
        put(&mut b, 348 + 24, 1); // clip on
        put(&mut b, 392, 21);
        put(&mut b, 396 + 52, 1); // bounds test
        put(&mut b, 456, 13);
        put(&mut b, 460, 1);
        let s = fixed_state(&state(b, true)).unwrap();
        assert!(s.alpha_to_coverage && !s.independent_blend);
        assert!(s.targets[0].enable && s.targets[0].op == 1);
        assert_eq!(s.sample_mask, 0xf0f0);
        assert_eq!((s.fill, s.cull, s.depth_clip), (0, 1, true));
        assert!(s.depth.bounds_enable);
        assert_eq!(s.strip_cut, 1);
        // without any state subobject: D3D12's defaults
        let mut only = vec![0u8; 8];
        put(&mut only, 0, 13);
        let d = fixed_state(&state(only, true)).unwrap();
        assert_eq!(d, FixedState::default());
        assert!(fixed_state(&state(vec![99, 0, 0, 0], true)).is_err());
        assert!(fixed_state(&state(vec![0; 100], false)).is_err());
    }
}
