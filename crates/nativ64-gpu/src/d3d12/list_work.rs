//! Command list methods that do work: draws, dispatches, copies, clears, barriers, queries and
//! the console's memory operations. Each records one [`Command`] (arrays copied); markers and
//! hints the backend has no use for do nothing.
//!
//! Slots are those of the title's "newer" layout (see `list.rs`): the PC's up to 58 (queries at
//! 28-31, clears at 51-55), then the console's from 61 on, two slots later than the older
//! layout's.

use std::ffi::c_void;

use crate::d3d12::command::{Barrier, Command, CopyLocation, Rect};
use crate::d3d12::list::{record, upload_memory};
use crate::d3d12::list_state::copy_items;
use crate::xobj::SlotTable;

pub const fn with_work<const N: usize>(t: SlotTable<N>) -> SlotTable<N> {
    t.with(12, draw as *const ())
        .with(13, draw_indexed as *const ())
        .with(14, dispatch as *const ())
        .with(15, copy_buffer_region as *const ())
        .with(16, copy_texture_region as *const ())
        .with(17, copy_resource as *const ())
        .with(19, resolve_subresource as *const ())
        .with(26, barriers as *const ())
        .with(28, begin_query as *const ())
        .with(29, end_query as *const ())
        .with(30, resolve_queries as *const ())
        .with(51, clear_depth_stencil as *const ())
        .with(52, clear_render_target as *const ())
        .with(53, clear_uav_uint as *const ())
        .with(54, clear_uav_float as *const ())
        .with(55, discard as *const ())
        // Markers and events (SetMarker, BeginEvent, EndEvent): debugging names only.
        .with(56, ignored as *const ())
        .with(57, ignored as *const ())
        .with(58, ignored as *const ())
        .with(61, execute_indirect as *const ())
        .with(62, copy_memory as *const ())
        .with(63, fill32 as *const ())
        .with(64, fill64 as *const ())
        .with(65, fill128 as *const ())
        .with(71, write32 as *const ())
        .with(72, write64 as *const ())
        .with(73, wait32 as *const ())
        .with(74, wait64 as *const ())
        // Pixel shader depth order hint (older 73).
        .with(75, ignored as *const ())
        .with(80, dispatch_x as *const ())
        .with(85, copy_texture_region_x as *const ())
        .with(86, copy_resource_x as *const ())
        .with(100, dispatch_mesh_x as *const ())
        .with(101, driver_hint as *const ())
        // Shading rate hint (older 124).
        .with(126, ignored as *const ())
        .with(129, dispatch_mesh as *const ())
        // 0x470 `(u64)`: purpose unknown, called with its result unused.
        .with(142, ignored as *const ())
        .with(146, barrier_groups as *const ())
        .with(148, allocate_upload as *const ())
}

/// A method with nothing to record: any arguments, nothing returned.
unsafe extern "win64" fn ignored(_this: *mut c_void) {}

/// Slot 12: `DrawInstanced(vertices, instances, first vertex, first instance)`.
unsafe extern "win64" fn draw(this: *mut c_void, v: u32, i: u32, first_vertex: u32, first_i: u32) {
    record(
        this,
        Command::Draw {
            vertices: v,
            instances: i,
            first_vertex,
            first_instance: first_i,
        },
    );
}

/// Slot 13: `DrawIndexedInstanced(indices, instances, first index, base vertex, first instance)`.
unsafe extern "win64" fn draw_indexed(
    this: *mut c_void,
    indices: u32,
    instances: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
) {
    record(
        this,
        Command::DrawIndexed {
            indices,
            instances,
            first_index,
            base_vertex,
            first_instance,
        },
    );
}

/// Slot 14: `Dispatch(x, y, z)`.
unsafe extern "win64" fn dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    record(this, Command::Dispatch { x, y, z, flags: 0 });
}

/// Slot 80: `DispatchX(x, y, z, flags)`.
unsafe extern "win64" fn dispatch_x(this: *mut c_void, x: u32, y: u32, z: u32, flags: u32) {
    record(this, Command::Dispatch { x, y, z, flags });
}

/// Slot 129: `DispatchMesh(x, y, z)`.
unsafe extern "win64" fn dispatch_mesh(this: *mut c_void, x: u32, y: u32, z: u32) {
    record(this, Command::DispatchMesh { x, y, z });
}

/// Slot 100: `DispatchMeshX(x, flags, indirect arguments, payload)`.
unsafe extern "win64" fn dispatch_mesh_x(
    this: *mut c_void,
    x: u32,
    flags: u64,
    indirect: u64,
    payload: u64,
) {
    record(
        this,
        Command::DispatchMeshX {
            x,
            flags,
            indirect,
            payload,
        },
    );
}

/// Slot 61: `ExecuteIndirect(signature, max count, arguments, offset, count buffer, offset)`.
unsafe extern "win64" fn execute_indirect(
    this: *mut c_void,
    signature: *mut c_void,
    max: u32,
    arguments: *mut c_void,
    offset: u64,
    count: *mut c_void,
    count_offset: u64,
) {
    record(
        this,
        Command::ExecuteIndirect {
            signature: signature as usize,
            max,
            arguments: arguments as usize,
            offset,
            count: count as usize,
            count_offset,
        },
    );
}

/// Slot 15: `CopyBufferRegion(dst, dst offset, src, src offset, bytes)`.
unsafe extern "win64" fn copy_buffer_region(
    this: *mut c_void,
    dst: *mut c_void,
    dst_offset: u64,
    src: *mut c_void,
    src_offset: u64,
    len: u64,
) {
    record(
        this,
        Command::CopyBufferRegion {
            dst: dst as usize,
            dst_offset,
            src: src as usize,
            src_offset,
            len,
        },
    );
}

/// `D3D12_TEXTURE_COPY_LOCATION` (48 bytes).
#[repr(C)]
struct RawLocation {
    resource: usize,
    kind: u32,
    /// The footprint's offset; its low half is also the subresource index.
    offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    row_pitch: u32,
}

unsafe fn location(raw: *const RawLocation) -> Option<CopyLocation> {
    let r = raw.as_ref()?;
    Some(match r.kind {
        0 => CopyLocation::Subresource {
            resource: r.resource,
            index: r.offset as u32,
        },
        1 => CopyLocation::Footprint {
            resource: r.resource,
            offset: r.offset,
            format: r.format,
            width: r.width,
            height: r.height,
            depth: r.depth,
            row_pitch: r.row_pitch,
        },
        kind => CopyLocation::Other {
            resource: r.resource,
            kind,
        },
    })
}

#[allow(clippy::too_many_arguments)] // the method's own arguments
unsafe fn copy_texture(
    this: *mut c_void,
    dst: *const RawLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const RawLocation,
    source_box: *const [u32; 6],
    flags: u32,
) {
    // Without both locations there is nothing to replay.
    let (Some(dst), Some(src)) = (location(dst), location(src)) else {
        return;
    };
    record(
        this,
        Command::CopyTextureRegion {
            dst,
            x,
            y,
            z,
            src,
            source_box: source_box.as_ref().copied(),
            flags,
        },
    );
}

/// Slot 16: `CopyTextureRegion(dst, x, y, z, src, box)`.
unsafe extern "win64" fn copy_texture_region(
    this: *mut c_void,
    dst: *const RawLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const RawLocation,
    source_box: *const [u32; 6],
) {
    copy_texture(this, dst, x, y, z, src, source_box, 0);
}

/// Slot 85: `CopyTextureRegionX(dst, x, y, z, src, box, flags)`.
#[allow(clippy::too_many_arguments)] // the method's own arguments
unsafe extern "win64" fn copy_texture_region_x(
    this: *mut c_void,
    dst: *const RawLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const RawLocation,
    source_box: *const [u32; 6],
    flags: u32,
) {
    copy_texture(this, dst, x, y, z, src, source_box, flags);
}

/// Slot 17: `CopyResource(dst, src)`.
unsafe extern "win64" fn copy_resource(this: *mut c_void, dst: *mut c_void, src: *mut c_void) {
    copy_resource_x(this, dst, src, 0);
}

/// Slot 86: `CopyResourceX(dst, src, flags)`.
unsafe extern "win64" fn copy_resource_x(
    this: *mut c_void,
    dst: *mut c_void,
    src: *mut c_void,
    flags: u32,
) {
    record(
        this,
        Command::CopyResource {
            dst: dst as usize,
            src: src as usize,
            flags,
        },
    );
}

/// Slot 19: `ResolveSubresource(dst, dst subresource, src, src subresource, format)`.
unsafe extern "win64" fn resolve_subresource(
    this: *mut c_void,
    dst: *mut c_void,
    dst_sub: u32,
    src: *mut c_void,
    src_sub: u32,
    format: u32,
) {
    record(
        this,
        Command::ResolveSubresource {
            dst: dst as usize,
            dst_sub,
            src: src as usize,
            src_sub,
            format,
        },
    );
}

/// Slot 62: `CopyMemoryToMemoryX(dst, src, bytes, flags)`.
unsafe extern "win64" fn copy_memory(this: *mut c_void, dst: u64, src: u64, len: u64, flags: u32) {
    record(
        this,
        Command::CopyMemory {
            dst,
            src,
            len,
            flags,
        },
    );
}

unsafe fn fill(this: *mut c_void, address: u64, size: u64, bytes: &[u8]) {
    let mut pattern = [0u8; 16];
    pattern[..bytes.len()].copy_from_slice(bytes);
    record(
        this,
        Command::FillMemory {
            address,
            size,
            pattern,
            width: bytes.len() as u8,
        },
    );
}
/// Slot 63: `FillMemoryWith32BitValueX(address, bytes, value)`.
unsafe extern "win64" fn fill32(this: *mut c_void, address: u64, size: u64, value: u32) {
    fill(this, address, size, &value.to_le_bytes());
}
/// Slot 64: `FillMemoryWith64BitValueX`.
unsafe extern "win64" fn fill64(this: *mut c_void, address: u64, size: u64, value: u64) {
    fill(this, address, size, &value.to_le_bytes());
}
/// Slot 65: `FillMemoryWith128BitValueX(address, bytes, &value)`; NULL records nothing.
unsafe extern "win64" fn fill128(
    this: *mut c_void,
    address: u64,
    size: u64,
    value: *const [u8; 16],
) {
    if let Some(v) = value.as_ref() {
        fill(this, address, size, v);
    }
}

/// Slot 71: `Write32BitValueBottomOfPipeX(address, value, flush, flags)`.
unsafe extern "win64" fn write32(
    this: *mut c_void,
    address: u64,
    value: u32,
    flush: u32,
    flags: u32,
) {
    record(
        this,
        Command::WriteBottom {
            address,
            value: u64::from(value),
            wide: false,
            flush,
            flags,
        },
    );
}
/// Slot 72: `Write64BitValueBottomOfPipeX`.
unsafe extern "win64" fn write64(
    this: *mut c_void,
    address: u64,
    value: u64,
    flush: u32,
    flags: u32,
) {
    record(
        this,
        Command::WriteBottom {
            address,
            value,
            wide: true,
            flush,
            flags,
        },
    );
}
/// Slot 73: `Wait32BitValueX(address, comparison, reference, mask, flags)`.
unsafe extern "win64" fn wait32(
    this: *mut c_void,
    address: u64,
    comparison: u32,
    reference: u32,
    mask: u32,
    _flags: u32,
) {
    record(
        this,
        Command::WaitValue {
            address,
            comparison,
            reference: u64::from(reference),
            mask: u64::from(mask),
            wide: false,
        },
    );
}
/// Slot 74: `Wait64BitValueX(address, comparison, reference, flags)`.
unsafe extern "win64" fn wait64(
    this: *mut c_void,
    address: u64,
    comparison: u32,
    reference: u64,
    _flags: u32,
) {
    record(
        this,
        Command::WaitValue {
            address,
            comparison,
            reference,
            mask: u64::MAX,
            wide: true,
        },
    );
}

/// Decodes one `D3D12_RESOURCE_BARRIER` (32 bytes).
unsafe fn barrier(raw: *const u8) -> Barrier {
    let u32_at = |o: usize| raw.add(o).cast::<u32>().read_unaligned();
    let ptr_at = |o: usize| raw.add(o).cast::<usize>().read_unaligned();
    let flags = u32_at(4);
    match u32_at(0) {
        0 => Barrier::Transition {
            flags,
            resource: ptr_at(8),
            subresource: u32_at(16),
            before: u32_at(20),
            after: u32_at(24),
        },
        1 => Barrier::Aliasing {
            flags,
            before: ptr_at(8),
            after: ptr_at(16),
        },
        2 => Barrier::Uav {
            flags,
            resource: ptr_at(8),
        },
        kind => Barrier::Other { kind, flags },
    }
}

/// Slot 26: `ResourceBarrier(count, barriers)`; returns the console's barrier "actions": none.
unsafe extern "win64" fn barriers(this: *mut c_void, count: u32, barriers: *const [u8; 32]) -> u32 {
    let list = if barriers.is_null() {
        Vec::new()
    } else {
        (0..count as usize)
            .map(|i| barrier(barriers.add(i).cast()))
            .collect()
    };
    record(this, Command::Barriers(list));
    0
}

/// Slot 146: the enhanced barriers `(group count, groups)`, 16-byte groups `(type, count,
/// barriers)`. Shortcut: only the type and count of each group are kept.
unsafe extern "win64" fn barrier_groups(this: *mut c_void, count: u32, groups: *const [u32; 4]) {
    let groups = copy_items(groups, count);
    record(
        this,
        Command::BarrierGroups(groups.iter().map(|g| (g[0], g[1])).collect()),
    );
}

/// Slot 55: `DiscardResource(resource, region)`; `D3D12_DISCARD_REGION` is
/// `(rect count, rects, first subresource, subresource count)`.
unsafe extern "win64" fn discard(
    this: *mut c_void,
    resource: *mut c_void,
    region: *const [usize; 3],
) {
    let (rects, first_subresource, subresources) = match region.as_ref() {
        Some(r) => (
            copy_items(r[1] as *const Rect, r[0] as u32),
            r[2] as u32,
            (r[2] >> 32) as u32,
        ),
        None => (Vec::new(), 0, 0),
    };
    record(
        this,
        Command::Discard {
            resource: resource as usize,
            rects,
            first_subresource,
            subresources,
        },
    );
}

/// Slot 52: `ClearRenderTargetView(handle, color, rect count, rects)`; no color records nothing.
unsafe extern "win64" fn clear_render_target(
    this: *mut c_void,
    handle: u64,
    color: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    let Some(&color) = color.as_ref() else {
        return;
    };
    record(
        this,
        Command::ClearRenderTarget {
            handle,
            color,
            rects: copy_items(rects, count),
        },
    );
}

/// Slot 51: `ClearDepthStencilView(handle, flags, depth, stencil, rect count, rects)`.
unsafe extern "win64" fn clear_depth_stencil(
    this: *mut c_void,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    record(
        this,
        Command::ClearDepthStencil {
            handle,
            flags,
            depth,
            stencil,
            rects: copy_items(rects, count),
        },
    );
}

#[allow(clippy::too_many_arguments)] // the method's own arguments
unsafe fn clear_uav(
    this: *mut c_void,
    float: bool,
    gpu_handle: u64,
    cpu_handle: u64,
    resource: *mut c_void,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    let Some(&values) = values.as_ref() else {
        return;
    };
    record(
        this,
        Command::ClearUav {
            float,
            gpu_handle,
            cpu_handle,
            resource: resource as usize,
            values,
            rects: copy_items(rects, count),
        },
    );
}
/// Slot 53: `ClearUnorderedAccessViewUint(GPU handle, CPU handle, resource, values, rect count,
/// rects)`.
unsafe extern "win64" fn clear_uav_uint(
    this: *mut c_void,
    gpu: u64,
    cpu: u64,
    resource: *mut c_void,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    clear_uav(this, false, gpu, cpu, resource, values, count, rects);
}
/// Slot 54: `ClearUnorderedAccessViewFloat`, the same with four floats (kept as bits).
unsafe extern "win64" fn clear_uav_float(
    this: *mut c_void,
    gpu: u64,
    cpu: u64,
    resource: *mut c_void,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    clear_uav(this, true, gpu, cpu, resource, values, count, rects);
}

/// Slot 28: `BeginQuery(heap, type, index)`.
unsafe extern "win64" fn begin_query(this: *mut c_void, heap: *mut c_void, kind: u32, index: u32) {
    record(
        this,
        Command::BeginQuery {
            heap: heap as usize,
            kind,
            index,
        },
    );
}
/// Slot 29: `EndQuery(heap, type, index)`.
unsafe extern "win64" fn end_query(this: *mut c_void, heap: *mut c_void, kind: u32, index: u32) {
    record(
        this,
        Command::EndQuery {
            heap: heap as usize,
            kind,
            index,
        },
    );
}
/// Slot 30: `ResolveQueryData(heap, type, first, count, buffer, offset)`.
unsafe extern "win64" fn resolve_queries(
    this: *mut c_void,
    heap: *mut c_void,
    kind: u32,
    first: u32,
    count: u32,
    buffer: *mut c_void,
    offset: u64,
) {
    record(
        this,
        Command::ResolveQueries {
            heap: heap as usize,
            kind,
            first,
            count,
            buffer: buffer as usize,
            offset,
        },
    );
}

/// Slot 101: `DriverHintX(feature, value)`: no hints; returns 0.
unsafe extern "win64" fn driver_hint(_this: *mut c_void, _feature: u64, _value: u64) -> u64 {
    0
}

/// Slot 148: `AllocateUploadMemoryX(size, alignment)`: list-owned memory the CPU writes and the
/// GPU reads at the same address, NULL on failure.
unsafe extern "win64" fn allocate_upload(this: *mut c_void, size: u64, alignment: u64) -> *mut u8 {
    upload_memory(this, size, alignment)
}
