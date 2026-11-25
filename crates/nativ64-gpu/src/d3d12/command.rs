//! The recorded command stream: one enum variant per command a list method records, with
//! resolved arguments (addresses as the title gave them, objects as pointers kept only for
//! identity). A backend replays it at `ExecuteCommandLists[X]`.
//!
//! Variants are added as the title's calls reach them (one per trapped list slot).

/// One recorded command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A pipeline state was bound (at list creation, `Reset`, or `SetPipelineState`).
    SetPipeline { pipeline: usize },
    /// A pipeline sync (`flags`, `control`, `address`, `size`) at the top (`bottom` false) or
    /// bottom of the pipe: waits for earlier work and cache flushes.
    Sync {
        bottom: bool,
        flags: u32,
        control: u32,
        address: u64,
        size: u64,
    },
}
