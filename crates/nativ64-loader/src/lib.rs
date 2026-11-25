//! Native PE32+ loader: parses an image, maps it, applies relocations and binds its imports
//! to `nativ64` exports (or to trap stubs). It does not execute guest code yet.

mod error;
mod image;
mod pe;
mod stubs;

pub use error::LoadError;
pub use image::{Image, ImportEntry, LoadOptions};
pub use pe::{DataDir, Pe, Section};
