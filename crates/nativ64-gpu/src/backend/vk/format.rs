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

/// The Vulkan format and byte size of a vertex attribute in DXGI format `dxgi` (typeless and
/// block formats are not vertex formats).
pub fn vertex(dxgi: u32) -> Option<(vk::Format, u32)> {
    use vk::Format as F;
    Some(match dxgi {
        2 => (F::R32G32B32A32_SFLOAT, 16),
        3 => (F::R32G32B32A32_UINT, 16),
        4 => (F::R32G32B32A32_SINT, 16),
        6 => (F::R32G32B32_SFLOAT, 12),
        7 => (F::R32G32B32_UINT, 12),
        8 => (F::R32G32B32_SINT, 12),
        10 => (F::R16G16B16A16_SFLOAT, 8),
        11 => (F::R16G16B16A16_UNORM, 8),
        12 => (F::R16G16B16A16_UINT, 8),
        13 => (F::R16G16B16A16_SNORM, 8),
        14 => (F::R16G16B16A16_SINT, 8),
        16 => (F::R32G32_SFLOAT, 8),
        17 => (F::R32G32_UINT, 8),
        18 => (F::R32G32_SINT, 8),
        24 => (F::A2B10G10R10_UNORM_PACK32, 4),
        25 => (F::A2B10G10R10_UINT_PACK32, 4),
        26 => (F::B10G11R11_UFLOAT_PACK32, 4),
        28 => (F::R8G8B8A8_UNORM, 4),
        29 => (F::R8G8B8A8_SRGB, 4),
        30 => (F::R8G8B8A8_UINT, 4),
        31 => (F::R8G8B8A8_SNORM, 4),
        32 => (F::R8G8B8A8_SINT, 4),
        34 => (F::R16G16_SFLOAT, 4),
        35 => (F::R16G16_UNORM, 4),
        36 => (F::R16G16_UINT, 4),
        37 => (F::R16G16_SNORM, 4),
        38 => (F::R16G16_SINT, 4),
        41 => (F::R32_SFLOAT, 4),
        42 => (F::R32_UINT, 4),
        43 => (F::R32_SINT, 4),
        49 => (F::R8G8_UNORM, 2),
        50 => (F::R8G8_UINT, 2),
        51 => (F::R8G8_SNORM, 2),
        52 => (F::R8G8_SINT, 2),
        54 => (F::R16_SFLOAT, 2),
        56 => (F::R16_UNORM, 2),
        57 => (F::R16_UINT, 2),
        58 => (F::R16_SNORM, 2),
        59 => (F::R16_SINT, 2),
        61 => (F::R8_UNORM, 1),
        62 => (F::R8_UINT, 1),
        63 => (F::R8_SNORM, 1),
        64 => (F::R8_SINT, 1),
        87 => (F::B8G8R8A8_UNORM, 4),
        _ => return None,
    })
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

    #[test]
    fn vertex_formats() {
        assert_eq!(vertex(2), Some((vk::Format::R32G32B32A32_SFLOAT, 16)));
        assert_eq!(vertex(28), Some((vk::Format::R8G8B8A8_UNORM, 4)));
        assert_eq!(vertex(1), None, "typeless");
        assert_eq!(vertex(71), None);
    }
}
