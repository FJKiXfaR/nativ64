//! SPIR-V emission (`docs/shaders.md`, SPIR-V emission; chunk 6 of the G4 plan).
//!
//! * `builder`: our own module builder over the `spirv` crate's enums.
//! * `alu`: the per-opcode semantics over a `Backend` of 32-bit primitives, with the CPU
//!   evaluator (`Cpu`) the unit tests run on.
//! * `emit`: the SPIR-V backend and the translation of a `Structured` program to the probe
//!   module (one `GLCompute` invocation = one lane).

pub mod alu;
pub mod builder;
pub mod emit;

pub use alu::Unsupported;
pub use emit::{translate, translate_io, Probe, SpirvError};

#[cfg(test)]
mod gpu;
#[cfg(test)]
mod tests;
