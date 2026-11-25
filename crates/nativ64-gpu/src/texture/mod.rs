//! Console texture layouts (GFX10.3): the memory layout of textures in the GPU's own tiled
//! formats. See `docs/textures.md`.

pub mod equation;
pub mod format;
pub mod layout;
pub mod tile;
#[rustfmt::skip]
#[allow(clippy::all)]
pub mod swizzle_tables;

pub use equation::{Config, Equation, EquationError, SwizzleMode, SERIES_S, SERIES_X};
pub use format::{format_info, FormatInfo, Planes};
pub use layout::{suggest_mode, Layout, LayoutError};
pub use tile::{detile, tile, Addresser, TileError};
