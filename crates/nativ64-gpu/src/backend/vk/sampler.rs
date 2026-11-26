//! Samplers (chunk 10b, `docs/shaders.md`): the static ones a stage writes as constant S#
//! words (`SamplerDesc`, immutable samplers of set 1) and the ones the title makes in a heap
//! (`View::Sampler`, the D3D12 description; `View::ConsoleSampler`, whose first 13 words are
//! read the same way), both reduced to one `Spec` and made once per distinct spec.
//!
//! shortcuts: a minimum or maximum reduction filter, a comparison sampler built from a hardware
//! S# (only the `_c` opcodes use it) and a border colour from the hardware's table are
//! approximated (weighted average, no comparison, transparent black) and logged once; the
//! "half border" and "mirror once" address modes of the hardware map to the nearest Vulkan mode
//! **[unverified]**.

use super::{log_once, Gpu};
use crate::shader::resource::SamplerDesc;
use ash::vk;
use std::collections::HashMap;

/// Everything a `VkSamplerCreateInfo` takes, in a form that is `Eq` and `Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Spec {
    pub mag_linear: bool,
    pub min_linear: bool,
    /// Mip filter: 0 base level only, 1 nearest, 2 linear.
    pub mip: u8,
    /// `VkSamplerAddressMode` raw values for U, V, W.
    pub address: [i32; 3],
    pub lod_bias: u32,
    pub min_lod: u32,
    pub max_lod: u32,
    /// Maximum anisotropy ratio (1: off).
    pub anisotropy: u8,
    /// `VkCompareOp` raw value of a comparison sampler.
    pub compare: Option<i32>,
    /// `VkBorderColor` raw value (0 transparent black, 2 opaque black, 4 opaque white, the
    /// float forms).
    pub border: i32,
}

const REPEAT: i32 = vk::SamplerAddressMode::REPEAT.as_raw();
const MIRROR: i32 = vk::SamplerAddressMode::MIRRORED_REPEAT.as_raw();
const EDGE: i32 = vk::SamplerAddressMode::CLAMP_TO_EDGE.as_raw();
const BORDER: i32 = vk::SamplerAddressMode::CLAMP_TO_BORDER.as_raw();
const MIRROR_EDGE: i32 = vk::SamplerAddressMode::MIRROR_CLAMP_TO_EDGE.as_raw();

const TRANSPARENT: i32 = vk::BorderColor::FLOAT_TRANSPARENT_BLACK.as_raw();
const OPAQUE_BLACK: i32 = vk::BorderColor::FLOAT_OPAQUE_BLACK.as_raw();
const OPAQUE_WHITE: i32 = vk::BorderColor::FLOAT_OPAQUE_WHITE.as_raw();

/// The largest LOD Vulkan needs (`VK_LOD_CLAMP_NONE`).
const LOD_NONE: f32 = 1000.0;

impl Spec {
    /// From a static sampler decoded from constant S# words.
    pub fn from_static(d: &SamplerDesc) -> Spec {
        if d.reduction != 0 {
            log_once(format!(
                "sampler: reduction filter {} is read as a weighted average",
                d.reduction
            ));
        }
        if d.border == 3 {
            log_once(
                "sampler: a border colour from the hardware table is read as transparent".into(),
            );
        }
        let address = d.address.map(|a| match a {
            0 => REPEAT,
            1 => MIRROR,
            2 => EDGE,
            3 | 5 | 7 => MIRROR_EDGE,
            _ => BORDER,
        });
        let aniso = matches!(d.mag_filter.max(d.min_filter), 2 | 3);
        // filters: 0 point, 1 bilinear, 2 anisotropic point, 3 anisotropic bilinear
        Spec {
            mag_linear: d.mag_filter & 1 != 0,
            min_linear: d.min_filter & 1 != 0,
            mip: match d.mip_filter {
                0 => 0,
                1 => 1,
                _ => 2,
            },
            address,
            lod_bias: d.lod_bias_f32().to_bits(),
            min_lod: d.min_lod_f32().to_bits(),
            max_lod: d.max_lod_f32().min(LOD_NONE).to_bits(),
            anisotropy: if aniso { d.max_anisotropy() as u8 } else { 1 },
            compare: None,
            border: match d.border {
                1 => OPAQUE_BLACK,
                2 => OPAQUE_WHITE,
                _ => TRANSPARENT,
            },
        }
    }

    /// From a `D3D12_SAMPLER_DESC` as 13 words: filter, address U, V, W, LOD bias, maximum
    /// anisotropy, comparison function, border colour (4 floats), minimum and maximum LOD.
    pub fn from_d3d12(w: &[u32; 13]) -> Spec {
        let f = |i: usize| f32::from_bits(w[i]);
        let filter = w[0];
        // the filter bits: 0x10 minification, 0x04 magnification, 0x01 mip linear, 0x40
        // anisotropic; bits 7-8 the reduction (1: comparison, 2: minimum, 3: maximum)
        let reduction = filter >> 7 & 3;
        if reduction >= 2 {
            log_once(format!(
                "sampler: reduction filter {reduction} is read as a weighted average"
            ));
        }
        let address = [w[1], w[2], w[3]].map(|a| match a {
            1 => REPEAT,
            2 => MIRROR,
            3 => EDGE,
            4 => BORDER,
            5 => MIRROR_EDGE,
            _ => REPEAT,
        });
        let anisotropic = filter & 0x40 != 0;
        let (r, g, b, a) = (f(7), f(8), f(9), f(10));
        let border = if a < 0.5 {
            TRANSPARENT
        } else if r.max(g).max(b) < 0.5 {
            OPAQUE_BLACK
        } else {
            OPAQUE_WHITE
        };
        let exact = [
            (0.0, 0.0, 0.0, 0.0),
            (0.0, 0.0, 0.0, 1.0),
            (1.0, 1.0, 1.0, 1.0),
        ];
        if !exact.contains(&(r, g, b, a)) {
            log_once(format!(
                "sampler: border colour ({r}, {g}, {b}, {a}) becomes the nearest of three"
            ));
        }
        let clean = |v: f32| {
            if v.is_nan() {
                0.0
            } else {
                v.clamp(-LOD_NONE, LOD_NONE)
            }
        };
        Spec {
            mag_linear: anisotropic || filter & 0x04 != 0,
            min_linear: anisotropic || filter & 0x10 != 0,
            mip: if filter & 0x01 != 0 || anisotropic {
                2
            } else {
                1
            },
            address,
            lod_bias: clean(f(4)).to_bits(),
            min_lod: clean(f(11)).to_bits(),
            max_lod: clean(f(12)).to_bits(),
            anisotropy: if anisotropic {
                w[5].clamp(1, 16) as u8
            } else {
                1
            },
            compare: (reduction == 1).then(|| (w[6].clamp(1, 8) - 1) as i32),
            border,
        }
    }

    /// What an index that is not a sampler holds (so no shader reads an unwritten one):
    /// bilinear, repeat, every mip level.
    pub fn default_sampler() -> Spec {
        Spec {
            mag_linear: true,
            min_linear: true,
            mip: 2,
            address: [REPEAT; 3],
            lod_bias: 0.0f32.to_bits(),
            min_lod: 0.0f32.to_bits(),
            max_lod: LOD_NONE.to_bits(),
            anisotropy: 1,
            compare: None,
            border: TRANSPARENT,
        }
    }

    /// The spec of a heap sampler view (None for other views).
    pub fn of_view(v: &crate::d3d12::descriptor::View) -> Option<Spec> {
        use crate::d3d12::descriptor::View;
        match v {
            View::Sampler(w) => Some(Spec::from_d3d12(w)),
            View::ConsoleSampler(w) => {
                log_once(format!(
                    "console sampler words (the first 13 are read as a D3D12 description): {w:#x?}"
                ));
                let mut d = [0u32; 13];
                d.copy_from_slice(&w[..13]);
                Some(Spec::from_d3d12(&d))
            }
            _ => None,
        }
    }
}

/// The samplers made so far, one per distinct spec.
pub struct SamplerCache<'g> {
    gpu: &'g Gpu,
    map: HashMap<Spec, Option<vk::Sampler>>,
}

impl<'g> SamplerCache<'g> {
    pub fn new(gpu: &'g Gpu) -> Self {
        SamplerCache {
            gpu,
            map: HashMap::new(),
        }
    }

    /// The sampler of `spec`, made on first use (None, logged once, when the device refuses).
    pub fn get(&mut self, spec: Spec) -> Option<vk::Sampler> {
        let gpu = self.gpu;
        *self.map.entry(spec).or_insert_with(|| make(gpu, &spec))
    }
}

impl Drop for SamplerCache<'_> {
    fn drop(&mut self) {
        for s in self.map.values().flatten() {
            // SAFETY: made by `make`; the queue is idle when the backend drops.
            unsafe { self.gpu.device.destroy_sampler(*s, None) };
        }
    }
}

fn make(gpu: &Gpu, s: &Spec) -> Option<vk::Sampler> {
    let filter = |linear| {
        if linear {
            vk::Filter::LINEAR
        } else {
            vk::Filter::NEAREST
        }
    };
    let mode = |m: i32| {
        if m == MIRROR_EDGE && !gpu.caps.mirror_clamp {
            vk::SamplerAddressMode::CLAMP_TO_EDGE
        } else {
            vk::SamplerAddressMode::from_raw(m)
        }
    };
    let max_lod = if s.mip == 0 {
        0.0
    } else {
        f32::from_bits(s.max_lod)
    };
    let mut ci = vk::SamplerCreateInfo::default()
        .mag_filter(filter(s.mag_linear))
        .min_filter(filter(s.min_linear))
        .mipmap_mode(if s.mip == 2 {
            vk::SamplerMipmapMode::LINEAR
        } else {
            vk::SamplerMipmapMode::NEAREST
        })
        .address_mode_u(mode(s.address[0]))
        .address_mode_v(mode(s.address[1]))
        .address_mode_w(mode(s.address[2]))
        .mip_lod_bias(f32::from_bits(s.lod_bias))
        .min_lod(f32::from_bits(s.min_lod).min(max_lod))
        .max_lod(max_lod)
        .border_color(vk::BorderColor::from_raw(s.border));
    if let (Some(max), true) = (gpu.caps.max_anisotropy, s.anisotropy > 1) {
        ci = ci
            .anisotropy_enable(true)
            .max_anisotropy(f32::from(s.anisotropy).min(max));
    }
    if let Some(op) = s.compare {
        ci = ci
            .compare_enable(true)
            .compare_op(vk::CompareOp::from_raw(op));
    }
    // SAFETY: a valid create info; destroyed by the cache's `Drop`.
    match unsafe { gpu.device.create_sampler(&ci, None) } {
        Ok(h) => Some(h),
        Err(e) => {
            log_once(format!("vkCreateSampler for {s:?}: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_sampler_maps_to_the_spec() {
        // trilinear 4x anisotropic wrap, as the console-compiled one of ps c1d5a4b8
        let d = SamplerDesc::from_words([0x400, 0x00c0_0000, 0x18f0_0000, 0]);
        let s = Spec::from_static(&d);
        assert_eq!(s.address, [REPEAT; 3]);
        assert_eq!((s.mag_linear, s.min_linear, s.mip), (true, true, 2));
        assert_eq!(s.anisotropy, 4);
        assert_eq!(f32::from_bits(s.max_lod), 12.0);
    }

    #[test]
    fn a_d3d12_sampler_maps_to_the_spec() {
        let mut w = [0u32; 13];
        w[0] = 0x10; // min linear, mag point, mip point
        w[1..4].copy_from_slice(&[3, 1, 4]);
        w[7..11].copy_from_slice(&[0.0f32, 0.0, 0.0, 1.0].map(f32::to_bits));
        w[12] = 1000.0f32.to_bits();
        let s = Spec::from_d3d12(&w);
        assert_eq!((s.mag_linear, s.min_linear, s.mip), (false, true, 1));
        assert_eq!(s.address, [EDGE, REPEAT, BORDER]);
        assert_eq!(s.border, OPAQUE_BLACK);
        assert_eq!(s.compare, None);
        w[0] = 0x80 | 0x55;
        w[5] = 8;
        w[6] = 2;
        let s = Spec::from_d3d12(&w);
        assert_eq!((s.anisotropy, s.compare), (8, Some(1)));
    }
}
