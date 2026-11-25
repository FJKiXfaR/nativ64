//! Command list methods that set state: fixed-function state, render targets, vertex and index
//! buffers, descriptor heaps and root arguments. Each records one [`Command`], with arrays the
//! title passes by pointer copied (it reuses that memory once the call returns).
//!
//! Slots are those of the title's layout (see `list.rs`): the PC's 11-27, the query methods
//! at 28-31, then `SetDescriptorHeaps` and the rest moved up by 4 (32-55).

use std::ffi::c_void;

use crate::d3d12::command::{Command, IndexView, RootView, StreamView, VertexView};
use crate::d3d12::list::record;
use crate::xobj::SlotTable;

/// `count` items at `items` copied, or none for NULL or 0.
///
/// # Safety
/// `items` is NULL or points to `count` readable values.
pub(crate) unsafe fn copy_items<T: Copy>(items: *const T, count: u32) -> Vec<T> {
    if items.is_null() || count == 0 {
        return Vec::new();
    }
    std::slice::from_raw_parts(items, count as usize).to_vec()
}

pub const fn with_state<const N: usize>(t: SlotTable<N>) -> SlotTable<N> {
    t.with(11, clear_state as *const ())
        .with(20, topology as *const ())
        .with(21, viewports as *const ())
        .with(22, scissors as *const ())
        .with(23, blend_factor as *const ())
        .with(24, stencil_ref as *const ())
        .with(25, set_pipeline as *const ())
        .with(31, predication as *const ())
        .with(32, heaps as *const ())
        .with(33, compute_signature as *const ())
        .with(34, graphics_signature as *const ())
        .with(35, compute_table as *const ())
        .with(36, graphics_table as *const ())
        .with(37, compute_constant as *const ())
        .with(38, graphics_constant as *const ())
        .with(39, compute_constants as *const ())
        .with(40, graphics_constants as *const ())
        .with(41, compute_cbv as *const ())
        .with(42, graphics_cbv as *const ())
        .with(43, compute_srv as *const ())
        .with(44, graphics_srv as *const ())
        .with(45, compute_uav as *const ())
        .with(46, graphics_uav as *const ())
        .with(47, index_buffer as *const ())
        .with(48, vertex_buffers as *const ())
        .with(49, stream_output as *const ())
        .with(50, render_targets as *const ())
        // Newer layout: the console's depth bounds (2 after the older slot 68).
        .with(70, depth_bounds as *const ())
}

/// Slot 11: `ClearState(pipeline)`.
unsafe extern "win64" fn clear_state(this: *mut c_void, pipeline: *mut c_void) {
    record(
        this,
        Command::ClearState {
            pipeline: pipeline as usize,
        },
    );
}

/// Slot 20: `IASetPrimitiveTopology(topology)`.
unsafe extern "win64" fn topology(this: *mut c_void, topology: u32) {
    record(this, Command::Topology(topology));
}

/// Slot 21: `RSSetViewports(count, viewports)`.
unsafe extern "win64" fn viewports(this: *mut c_void, count: u32, viewports: *const [f32; 6]) {
    record(this, Command::Viewports(copy_items(viewports, count)));
}

/// Slot 22: `RSSetScissorRects(count, rects)`.
unsafe extern "win64" fn scissors(this: *mut c_void, count: u32, rects: *const [i32; 4]) {
    record(this, Command::Scissors(copy_items(rects, count)));
}

/// Slot 23: `OMSetBlendFactor(factor)`.
unsafe extern "win64" fn blend_factor(this: *mut c_void, factor: *const [f32; 4]) {
    record(this, Command::BlendFactor(factor.as_ref().copied()));
}

/// Slot 24: `OMSetStencilRef(reference)`.
unsafe extern "win64" fn stencil_ref(this: *mut c_void, reference: u32) {
    record(this, Command::StencilRef(reference));
}

/// Slot 25: `SetPipelineState(pipeline)`.
unsafe extern "win64" fn set_pipeline(this: *mut c_void, pipeline: *mut c_void) {
    record(
        this,
        Command::SetPipeline {
            pipeline: pipeline as usize,
        },
    );
}

/// Slot 31: `SetPredication(buffer, offset, op)`.
unsafe extern "win64" fn predication(this: *mut c_void, buffer: *mut c_void, offset: u64, op: u32) {
    record(
        this,
        Command::Predication {
            buffer: buffer as usize,
            offset,
            op,
        },
    );
}

/// Slot 32: `SetDescriptorHeaps(count, heaps)`.
unsafe extern "win64" fn heaps(this: *mut c_void, count: u32, heaps: *const usize) {
    record(this, Command::Heaps(copy_items(heaps, count)));
}

unsafe fn signature(this: *mut c_void, compute: bool, signature: *mut c_void) {
    record(
        this,
        Command::RootSignature {
            compute,
            signature: signature as usize,
        },
    );
}
/// Slot 33: `SetComputeRootSignature`.
unsafe extern "win64" fn compute_signature(this: *mut c_void, sig: *mut c_void) {
    signature(this, true, sig);
}
/// Slot 34: `SetGraphicsRootSignature`.
unsafe extern "win64" fn graphics_signature(this: *mut c_void, sig: *mut c_void) {
    signature(this, false, sig);
}

unsafe fn table(this: *mut c_void, compute: bool, slot: u32, handle: u64) {
    record(
        this,
        Command::RootTable {
            compute,
            slot,
            handle,
        },
    );
}
/// Slot 35: `SetComputeRootDescriptorTable(slot, GPU handle)`.
unsafe extern "win64" fn compute_table(this: *mut c_void, slot: u32, handle: u64) {
    table(this, true, slot, handle);
}
/// Slot 36: `SetGraphicsRootDescriptorTable`.
unsafe extern "win64" fn graphics_table(this: *mut c_void, slot: u32, handle: u64) {
    table(this, false, slot, handle);
}

unsafe fn constants(this: *mut c_void, compute: bool, slot: u32, offset: u32, values: Vec<u32>) {
    record(
        this,
        Command::RootConstants {
            compute,
            slot,
            offset,
            values,
        },
    );
}
/// Slot 37: `SetComputeRoot32BitConstant(slot, value, offset)`.
unsafe extern "win64" fn compute_constant(this: *mut c_void, slot: u32, value: u32, offset: u32) {
    constants(this, true, slot, offset, vec![value]);
}
/// Slot 38: `SetGraphicsRoot32BitConstant`.
unsafe extern "win64" fn graphics_constant(this: *mut c_void, slot: u32, value: u32, offset: u32) {
    constants(this, false, slot, offset, vec![value]);
}
/// Slot 39: `SetComputeRoot32BitConstants(slot, count, values, offset)`.
unsafe extern "win64" fn compute_constants(
    this: *mut c_void,
    slot: u32,
    count: u32,
    values: *const u32,
    offset: u32,
) {
    constants(this, true, slot, offset, copy_items(values, count));
}
/// Slot 40: `SetGraphicsRoot32BitConstants`.
unsafe extern "win64" fn graphics_constants(
    this: *mut c_void,
    slot: u32,
    count: u32,
    values: *const u32,
    offset: u32,
) {
    constants(this, false, slot, offset, copy_items(values, count));
}

unsafe fn view(this: *mut c_void, compute: bool, view: RootView, slot: u32, address: u64) {
    record(
        this,
        Command::RootView {
            compute,
            slot,
            view,
            address,
        },
    );
}
/// Slots 41-46: `Set{Compute,Graphics}RootConstantBufferView`, `...ShaderResourceView`,
/// `...UnorderedAccessView` `(slot, GPU address)`.
unsafe extern "win64" fn compute_cbv(this: *mut c_void, slot: u32, address: u64) {
    view(this, true, RootView::Cbv, slot, address);
}
unsafe extern "win64" fn graphics_cbv(this: *mut c_void, slot: u32, address: u64) {
    view(this, false, RootView::Cbv, slot, address);
}
unsafe extern "win64" fn compute_srv(this: *mut c_void, slot: u32, address: u64) {
    view(this, true, RootView::Srv, slot, address);
}
unsafe extern "win64" fn graphics_srv(this: *mut c_void, slot: u32, address: u64) {
    view(this, false, RootView::Srv, slot, address);
}
unsafe extern "win64" fn compute_uav(this: *mut c_void, slot: u32, address: u64) {
    view(this, true, RootView::Uav, slot, address);
}
unsafe extern "win64" fn graphics_uav(this: *mut c_void, slot: u32, address: u64) {
    view(this, false, RootView::Uav, slot, address);
}

/// Slot 47: `IASetIndexBuffer(view)`.
unsafe extern "win64" fn index_buffer(this: *mut c_void, view: *const IndexView) {
    record(this, Command::IndexBuffer(view.as_ref().copied()));
}

/// Slot 48: `IASetVertexBuffers(start, count, views)`; NULL views unbind.
unsafe extern "win64" fn vertex_buffers(
    this: *mut c_void,
    start: u32,
    count: u32,
    views: *const VertexView,
) {
    record(
        this,
        Command::VertexBuffers {
            start,
            count,
            views: copy_items(views, count),
        },
    );
}

/// Slot 49: `SOSetTargets(start, count, views)`.
unsafe extern "win64" fn stream_output(
    this: *mut c_void,
    start: u32,
    count: u32,
    views: *const StreamView,
) {
    record(
        this,
        Command::StreamOutput {
            start,
            count,
            views: copy_items(views, count),
        },
    );
}

/// Slot 50: `OMSetRenderTargets(count, handles, single range, depth handle)`.
unsafe extern "win64" fn render_targets(
    this: *mut c_void,
    count: u32,
    handles: *const u64,
    single_range: i32,
    depth: *const u64,
) {
    let single_range = single_range != 0;
    let handles = if single_range {
        copy_items(handles, count.min(1))
    } else {
        copy_items(handles, count)
    };
    record(
        this,
        Command::RenderTargets {
            count,
            handles,
            single_range,
            depth: depth.as_ref().copied(),
        },
    );
}

/// Slot 70 (older 68): `OMSetDepthBounds(min, max)`.
unsafe extern "win64" fn depth_bounds(this: *mut c_void, min: f32, max: f32) {
    record(this, Command::DepthBounds { min, max });
}
