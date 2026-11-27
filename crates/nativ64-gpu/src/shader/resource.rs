//! What a translated stage hands the backend besides its SPIR-V words (`docs/shaders.md`,
//! Resources and chunk 9a): the static samplers the code writes as constant S# words, decoded
//! to a description the backend turns into an immutable sampler.

/// A sampler decoded from four constant RDNA2 S# words (`SQ_IMG_SAMP_WORD0..3`). The fields
/// keep the hardware's integer codes so the description is `Eq` and `Hash`; the helpers give
/// the numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SamplerDesc {
    /// Address modes U, V, W: 0 wrap, 1 mirror, 2 clamp to the last texel, 3 mirror once,
    /// 4 clamp to half the border, 5 mirror once to half the border, 6 clamp to the border,
    /// 7 mirror once to the border.
    pub address: [u8; 3],
    /// Magnification and minification filters: 0 point, 1 bilinear, 2 anisotropic point,
    /// 3 anisotropic bilinear.
    pub mag_filter: u8,
    pub min_filter: u8,
    /// Mip filter: 0 none (base level only), 1 point, 2 linear.
    pub mip_filter: u8,
    /// log2 of the maximum anisotropy (0..4 meaningful).
    pub aniso_log2: u8,
    /// Depth compare function: 0 never, 1 less, 2 equal, 3 less or equal, 4 greater,
    /// 5 not equal, 6 greater or equal, 7 always. Only the `_c` opcodes use it.
    pub compare: u8,
    /// Reduction: 0 weighted average, 1 minimum, 2 maximum.
    pub reduction: u8,
    /// Signed 14-bit LOD bias, in 1/256.
    pub lod_bias: i16,
    /// Minimum and maximum LOD, unsigned 12-bit, in 1/256.
    pub min_lod: u16,
    pub max_lod: u16,
    /// Border colour: 0 transparent black, 1 opaque black, 2 opaque white, 3 a table entry.
    pub border: u8,
}

impl SamplerDesc {
    /// Decodes the four words (field layout: the public `SQ_IMG_SAMP_WORD0..3`).
    pub fn from_words(w: [u32; 4]) -> Self {
        let bits = |word: u32, at: u32, n: u32| (word >> at & ((1 << n) - 1)) as u8;
        let wide = |word: u32, at: u32, n: u32| word >> at & ((1 << n) - 1);
        Self {
            address: [0, 3, 6].map(|at| bits(w[0], at, 3)),
            mag_filter: bits(w[2], 20, 2),
            min_filter: bits(w[2], 22, 2),
            mip_filter: bits(w[2], 26, 2),
            aniso_log2: bits(w[0], 9, 3),
            compare: bits(w[0], 12, 3),
            reduction: bits(w[0], 29, 2),
            lod_bias: ((wide(w[2], 0, 14) << 18) as i32 >> 18) as i16,
            min_lod: wide(w[1], 0, 12) as u16,
            max_lod: wide(w[1], 12, 12) as u16,
            border: bits(w[3], 30, 2),
        }
    }

    pub fn lod_bias_f32(&self) -> f32 {
        f32::from(self.lod_bias) / 256.0
    }

    pub fn min_lod_f32(&self) -> f32 {
        f32::from(self.min_lod) / 256.0
    }

    pub fn max_lod_f32(&self) -> f32 {
        f32::from(self.max_lod) / 256.0
    }

    /// The maximum anisotropy ratio (1, 2, 4, 8 or 16).
    pub fn max_anisotropy(&self) -> u32 {
        1 << self.aniso_log2.min(4)
    }
}

/// A translated stage: the module and the static samplers it declares, `samplers[k]` being
/// set 1, binding k.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Translated {
    pub words: Vec<u32>,
    pub samplers: Vec<SamplerDesc>,
    /// The entry reads a texture or a sampler of the bindless set (set 0, binding 0 or 2) or a
    /// static sampler: the draw needs images (chunk 10b).
    pub images: bool,
    /// The entry reads or writes a storage image of the bindless set (set 0, binding 1: a UAV
    /// texture, chunk 10c); `images` is set too.
    pub storage_images: bool,
    /// A raw scalar load or a built buffer has a base that does not trace to user data (an
    /// address read from memory): it reaches the GPU untranslated, so the backend skips the
    /// pipeline.
    pub memory_addresses: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_console_compiled_sampler() {
        // the words of a trilinear 4x anisotropic wrap sampler
        let d = SamplerDesc::from_words([0x400, 0x00c0_0000, 0x18f0_0000, 0]);
        assert_eq!(d.address, [0, 0, 0]);
        assert_eq!((d.mag_filter, d.min_filter, d.mip_filter), (3, 3, 2));
        assert_eq!((d.aniso_log2, d.max_anisotropy()), (2, 4));
        assert_eq!((d.min_lod, d.max_lod), (0, 0xc00));
        assert_eq!(d.max_lod_f32(), 12.0);
        assert_eq!((d.lod_bias, d.border, d.reduction), (0, 0, 0));
    }

    #[test]
    fn lod_bias_is_signed() {
        let d = SamplerDesc::from_words([0, 0, 0x3f00 | 0x00f0_0000, 0xc000_0000]);
        assert_eq!(d.lod_bias, -256);
        assert_eq!(d.lod_bias_f32(), -1.0);
        assert_eq!(d.border, 3);
        assert_eq!((d.mag_filter, d.min_filter), (3, 3));
    }
}
