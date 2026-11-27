//! Native PE32+ loader: parses an image, maps it, applies relocations and binds its imports
//! to `nativ64` exports (or to trap stubs), and runs its entry point on a thread with a
//! fake TEB/PEB and static TLS.

mod error;
mod exports;
pub mod fault;
pub use patches::{apply as apply_patches, parse as parse_patches, Patch};
mod image;
mod patches;
mod pe;
mod stubs;
pub mod teb;
mod threads;
mod tls;

pub use error::LoadError;
pub use image::{Image, ImportEntry, LoadOptions};
pub use pe::{DataDir, Pe, Section};
pub use stubs::trace_calls;
