//! Texel formats: element size, block compression and planes, per DXGI format number.
//!
//! An element is a texel, or a 4 x 4 block of texels for the BC formats. Console-only formats
//! (116-120, 189-191) come with the titles that use them: until then they are unknown.

/// How a format's data is split into planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Planes {
    /// One colour (or depth) plane.
    One,
    /// A depth plane of `element_bytes`, then a stencil plane of one byte per texel.
    DepthStencil,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    /// Bytes per element (of the depth plane for depth + stencil formats).
    pub element_bytes: u32,
    /// Texels per element along x and y: (4, 4) for BC, (1, 1) otherwise.
    pub block: (u32, u32),
    pub planes: Planes,
    /// Depth formats (with or without stencil): the ones with a metadata plane when used as a
    /// depth buffer.
    pub depth: bool,
}

const fn color(element_bytes: u32) -> Option<FormatInfo> {
    Some(FormatInfo {
        element_bytes,
        block: (1, 1),
        planes: Planes::One,
        depth: false,
    })
}

const fn depth(element_bytes: u32) -> Option<FormatInfo> {
    Some(FormatInfo {
        element_bytes,
        block: (1, 1),
        planes: Planes::One,
        depth: true,
    })
}

const fn depth_stencil(depth_bytes: u32) -> Option<FormatInfo> {
    Some(FormatInfo {
        element_bytes: depth_bytes,
        block: (1, 1),
        planes: Planes::DepthStencil,
        depth: true,
    })
}

const fn bc(element_bytes: u32) -> Option<FormatInfo> {
    Some(FormatInfo {
        element_bytes,
        block: (4, 4),
        planes: Planes::One,
        depth: false,
    })
}

/// The description of DXGI format `format`, or `None` for formats we do not lay out (unknown,
/// 1-bit, packed 4:2:2, video, console-only ones not met yet).
pub fn format_info(format: u32) -> Option<FormatInfo> {
    match format {
        1..=4 => color(16),
        5..=8 => color(12),
        9..=14 => color(8),
        15..=18 => color(8),
        19..=22 => depth_stencil(4),
        23..=26 => color(4),
        27..=32 => color(4),
        33..=38 => color(4),
        39 | 41..=43 => color(4),
        40 => depth(4),
        44..=47 => depth_stencil(4),
        48..=54 => color(2),
        55 => depth(2),
        56..=59 => color(2),
        60..=65 => color(1),
        67 => color(4),
        70..=72 | 79..=81 => bc(8),
        73..=78 | 82..=84 | 94..=99 => bc(16),
        85..=86 => color(2),
        87..=93 => color(4),
        115 => color(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_formats() {
        let rgba8 = format_info(28).unwrap();
        assert_eq!((rgba8.element_bytes, rgba8.block), (4, (1, 1)));
        assert_eq!(format_info(61).unwrap().element_bytes, 1);
        assert_eq!(format_info(10).unwrap().element_bytes, 8);
        assert_eq!(format_info(2).unwrap().element_bytes, 16);
        assert_eq!(format_info(87).unwrap().element_bytes, 4);
        assert_eq!(format_info(115).unwrap().element_bytes, 2);
    }

    #[test]
    fn block_compressed() {
        for (f, bytes) in [
            (71, 8),
            (74, 16),
            (77, 16),
            (80, 8),
            (83, 16),
            (95, 16),
            (98, 16),
        ] {
            let i = format_info(f).unwrap();
            assert_eq!((i.element_bytes, i.block), (bytes, (4, 4)), "{f}");
        }
    }

    #[test]
    fn depth_formats() {
        assert_eq!(format_info(40).unwrap().planes, Planes::One);
        assert!(format_info(40).unwrap().depth);
        assert!(format_info(55).unwrap().depth);
        for f in [20, 45] {
            let i = format_info(f).unwrap();
            assert_eq!(
                (i.planes, i.element_bytes, i.depth),
                (Planes::DepthStencil, 4, true)
            );
        }
        assert!(!format_info(41).unwrap().depth);
    }

    #[test]
    fn unknown_formats() {
        for f in [0, 66, 68, 69, 103, 113, 116, 120, 189, 191, 500] {
            assert!(format_info(f).is_none(), "{f}");
        }
    }
}
