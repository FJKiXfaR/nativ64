//! The recorded command stream: one enum variant per command a list method records, with
//! resolved arguments (addresses as the title gave them, objects as pointers kept only for
//! identity). A backend replays it at `ExecuteCommandLists[X]`.
//!
//! Arrays and structs the title passes by pointer are copied into the variant, since it reuses
//! that memory as soon as the call returns. Objects (resources, pipelines, root signatures,
//! heaps, query heaps, command signatures) are `usize` pointers: identity only.

/// A resource barrier, decoded from the 32-byte `D3D12_RESOURCE_BARRIER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Barrier {
    Transition {
        flags: u32,
        resource: usize,
        subresource: u32,
        before: u32,
        after: u32,
    },
    Aliasing {
        flags: u32,
        before: usize,
        after: usize,
    },
    Uav {
        flags: u32,
        resource: usize,
    },
    /// A type we do not know.
    Other {
        kind: u32,
        flags: u32,
    },
}

/// One side of a texture copy (`D3D12_TEXTURE_COPY_LOCATION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyLocation {
    Subresource {
        resource: usize,
        index: u32,
    },
    Footprint {
        resource: usize,
        offset: u64,
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        row_pitch: u32,
    },
    Other {
        resource: usize,
        kind: u32,
    },
}

/// `D3D12_INDEX_BUFFER_VIEW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexView {
    pub address: u64,
    pub size: u32,
    pub format: u32,
}

/// `D3D12_VERTEX_BUFFER_VIEW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexView {
    pub address: u64,
    pub size: u32,
    pub stride: u32,
}

/// `D3D12_STREAM_OUTPUT_BUFFER_VIEW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamView {
    pub address: u64,
    pub size: u64,
    pub filled_size_address: u64,
}

/// The kind of a root view (`SetXRootConstantBufferView`, `...ShaderResourceView`,
/// `...UnorderedAccessView`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootView {
    Cbv,
    Srv,
    Uav,
}

/// A rectangle (`left`, `top`, `right`, `bottom`).
pub type Rect = [i32; 4];

/// One recorded command.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A pipeline state was bound (at list creation, `Reset`, or `SetPipelineState`).
    SetPipeline {
        pipeline: usize,
    },
    /// `ClearState(pipeline)`: pipeline and bindings back to defaults, then the pipeline.
    ClearState {
        pipeline: usize,
    },
    /// A pipeline sync (`flags`, `control`, `address`, `size`) at the top (`bottom` false) or
    /// bottom of the pipe: waits for earlier work and cache flushes.
    Sync {
        bottom: bool,
        flags: u32,
        control: u32,
        address: u64,
        size: u64,
    },

    // Draws and dispatches.
    Draw {
        vertices: u32,
        instances: u32,
        first_vertex: u32,
        first_instance: u32,
    },
    DrawIndexed {
        indices: u32,
        instances: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
    },
    /// `Dispatch` (flags 0) and the console's `DispatchX`.
    Dispatch {
        x: u32,
        y: u32,
        z: u32,
        flags: u32,
    },
    DispatchMesh {
        x: u32,
        y: u32,
        z: u32,
    },
    /// `DispatchMeshX(x, flags, indirect arguments address, payload address)`.
    DispatchMeshX {
        x: u32,
        flags: u64,
        indirect: u64,
        payload: u64,
    },
    ExecuteIndirect {
        signature: usize,
        max: u32,
        arguments: usize,
        offset: u64,
        count: usize,
        count_offset: u64,
    },

    // Copies and fills.
    CopyBufferRegion {
        dst: usize,
        dst_offset: u64,
        src: usize,
        src_offset: u64,
        len: u64,
    },
    CopyTextureRegion {
        dst: CopyLocation,
        x: u32,
        y: u32,
        z: u32,
        src: CopyLocation,
        /// `left, top, front, right, bottom, back`; `None` is the whole source.
        source_box: Option<[u32; 6]>,
        flags: u32,
    },
    CopyResource {
        dst: usize,
        src: usize,
        flags: u32,
    },
    ResolveSubresource {
        dst: usize,
        dst_sub: u32,
        src: usize,
        src_sub: u32,
        format: u32,
    },
    /// `CopyMemoryToMemoryX`: between GPU addresses.
    CopyMemory {
        dst: u64,
        src: u64,
        len: u64,
        flags: u32,
    },
    /// `FillMemoryWith{32,64,128}BitValueX`: `pattern[..width]` repeated over `size` bytes.
    FillMemory {
        address: u64,
        size: u64,
        pattern: [u8; 16],
        width: u8,
    },
    /// `Write{32,64}BitValueBottomOfPipeX` (flags 1 and 2 write a timestamp).
    WriteBottom {
        address: u64,
        value: u64,
        wide: bool,
        flush: u32,
        flags: u32,
    },
    /// `Wait{32,64}BitValueX`.
    WaitValue {
        address: u64,
        comparison: u32,
        reference: u64,
        mask: u64,
        wide: bool,
    },

    // Barriers and discards.
    Barriers(Vec<Barrier>),
    /// Enhanced barrier groups `(type, barrier count)`; their contents are not decoded.
    BarrierGroups(Vec<(u32, u32)>),
    Discard {
        resource: usize,
        rects: Vec<Rect>,
        first_subresource: u32,
        subresources: u32,
    },

    // Clears.
    ClearRenderTarget {
        handle: u64,
        color: [f32; 4],
        rects: Vec<Rect>,
    },
    ClearDepthStencil {
        handle: u64,
        /// 1 depth, 2 stencil.
        flags: u32,
        depth: f32,
        stencil: u8,
        rects: Vec<Rect>,
    },
    ClearUav {
        float: bool,
        gpu_handle: u64,
        cpu_handle: u64,
        resource: usize,
        values: [u32; 4],
        rects: Vec<Rect>,
    },

    // Fixed-function state.
    Topology(u32),
    /// `x, y, width, height, min depth, max depth` each.
    Viewports(Vec<[f32; 6]>),
    Scissors(Vec<Rect>),
    /// `None`: the title passed NULL (the pipeline's default).
    BlendFactor(Option<[f32; 4]>),
    StencilRef(u32),
    DepthBounds {
        min: f32,
        max: f32,
    },
    IndexBuffer(Option<IndexView>),
    /// `views` is empty when the title passed NULL (unbind `count` slots from `start`).
    VertexBuffers {
        start: u32,
        count: u32,
        views: Vec<VertexView>,
    },
    StreamOutput {
        start: u32,
        count: u32,
        views: Vec<StreamView>,
    },
    /// `handles` as given; with `single_range` it holds only the first of `count` consecutive
    /// descriptors.
    RenderTargets {
        count: u32,
        handles: Vec<u64>,
        single_range: bool,
        depth: Option<u64>,
    },
    Predication {
        buffer: usize,
        offset: u64,
        op: u32,
    },

    // Descriptor heaps and root arguments.
    Heaps(Vec<usize>),
    RootSignature {
        compute: bool,
        signature: usize,
    },
    RootTable {
        compute: bool,
        slot: u32,
        handle: u64,
    },
    /// One or more 32-bit constants at `offset` (in constants) in root parameter `slot`.
    RootConstants {
        compute: bool,
        slot: u32,
        offset: u32,
        values: Vec<u32>,
    },
    RootView {
        compute: bool,
        slot: u32,
        view: RootView,
        address: u64,
    },

    // Queries.
    BeginQuery {
        heap: usize,
        kind: u32,
        index: u32,
    },
    EndQuery {
        heap: usize,
        kind: u32,
        index: u32,
    },
    ResolveQueries {
        heap: usize,
        kind: u32,
        first: u32,
        count: u32,
        buffer: usize,
        offset: u64,
    },
}
