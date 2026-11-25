//! DXGI formats to Vulkan formats, for the images the replay creates. Typeless formats take
//! the usual typed reading (UNORM, or float for 32-bit channels); a depth-stencil resource
//! takes the depth format of its size. Block-compressed and integer formats are not mapped
//! yet: a resource in one of them has no image and its commands are skipped.

use ash::vk;

/// What the replay needs to know about a DXGI format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    pub format: vk::Format,
    pub aspect: vk::ImageAspectFlags,
    /// Bytes per texel (not meaningful for depth-stencil copies).
    pub bytes: u32,
}

const COLOR: vk::ImageAspectFlags = vk::ImageAspectFlags::COLOR;
const DEPTH: vk::ImageAspectFlags = vk::ImageAspectFlags::DEPTH;
const DEPTH_STENCIL: vk::ImageAspectFlags = vk::ImageAspectFlags::from_raw(
    vk::ImageAspectFlags::DEPTH.as_raw() | vk::ImageAspectFlags::STENCIL.as_raw(),
);

/// `D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET`.
pub const FLAG_RENDER_TARGET: u32 = 1;
/// `D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL`.
pub const FLAG_DEPTH_STENCIL: u32 = 2;

fn info(format: vk::Format, aspect: vk::ImageAspectFlags, bytes: u32) -> Option<FormatInfo> {
    Some(FormatInfo {
        format,
        aspect,
        bytes,
    })
}

/// The Vulkan side of DXGI format `dxgi` for a resource created with `flags`.
pub fn lookup(dxgi: u32, flags: u32) -> Option<FormatInfo> {
    use vk::Format as F;
    let ds = flags & FLAG_DEPTH_STENCIL != 0;
    match dxgi {
        1 | 2 => info(F::R32G32B32A32_SFLOAT, COLOR, 16),
        9 | 10 => info(F::R16G16B16A16_SFLOAT, COLOR, 8),
        11 => info(F::R16G16B16A16_UNORM, COLOR, 8),
        15 | 16 => info(F::R32G32_SFLOAT, COLOR, 8),
        19 | 20 => info(F::D32_SFLOAT_S8_UINT, DEPTH_STENCIL, 8),
        23 | 24 => info(F::A2B10G10R10_UNORM_PACK32, COLOR, 4),
        26 => info(F::B10G11R11_UFLOAT_PACK32, COLOR, 4),
        27 | 28 => info(F::R8G8B8A8_UNORM, COLOR, 4),
        29 => info(F::R8G8B8A8_SRGB, COLOR, 4),
        33 | 34 => info(F::R16G16_SFLOAT, COLOR, 4),
        35 => info(F::R16G16_UNORM, COLOR, 4),
        39 if ds => info(F::D32_SFLOAT, DEPTH, 4),
        39 | 41 => info(F::R32_SFLOAT, COLOR, 4),
        40 => info(F::D32_SFLOAT, DEPTH, 4),
        44 | 45 => info(F::D32_SFLOAT_S8_UINT, DEPTH_STENCIL, 8),
        48 | 49 => info(F::R8G8_UNORM, COLOR, 2),
        53 if ds => info(F::D16_UNORM, DEPTH, 2),
        53 | 56 => info(F::R16_UNORM, COLOR, 2),
        54 => info(F::R16_SFLOAT, COLOR, 2),
        55 => info(F::D16_UNORM, DEPTH, 2),
        60 | 61 => info(F::R8_UNORM, COLOR, 1),
        87 | 90 => info(F::B8G8R8A8_UNORM, COLOR, 4),
        91 => info(F::B8G8R8A8_SRGB, COLOR, 4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_formats() {
        let f = lookup(28, FLAG_RENDER_TARGET).unwrap();
        assert_eq!(f.format, vk::Format::R8G8B8A8_UNORM);
        assert_eq!(f.bytes, 4);
        assert_eq!(
            lookup(39, FLAG_DEPTH_STENCIL).unwrap().format,
            vk::Format::D32_SFLOAT
        );
        assert_eq!(lookup(39, 0).unwrap().format, vk::Format::R32_SFLOAT);
        assert!(lookup(71, 0).is_none(), "BC1 is not mapped");
    }
}
