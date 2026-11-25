//! Native PE32+ loader: parses an image, maps it, applies relocations and binds its imports
//! to `nativ64` exports (or to trap stubs), and runs its entry point on a thread with a
//! fake TEB/PEB and static TLS.

mod error;
pub mod fault;
mod image;
mod pe;
mod stubs;
pub mod teb;
mod tls;

pub use error::LoadError;
pub use image::{Image, ImportEntry, LoadOptions};
pub use pe::{DataDir, Pe, Section};
