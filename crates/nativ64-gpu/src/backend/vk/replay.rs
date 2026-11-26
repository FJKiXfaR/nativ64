//! Replay of the title's recorded command streams (G3): render targets and textures as
//! device-local images made on first use, clears, copies and barriers executed, draws replayed
//! when they can be (`draw.rs`, chunk 10a), dispatches, mesh draws and indirect draws counted
//! and skipped. Images are always in `GENERAL` layout, so a title barrier is one conservative
//! global memory barrier, never a layout change.
//!
//! Buffers are the title's own memory, so buffer copies are plain CPU copies at record time,
//! in order with the staging of uploads (an upload reads its source bytes while recording).
//! Every submission waits for the GPU to be idle.

use super::format::{self, FormatInfo};
use super::{log_once, vk_try, Gpu, Result, VkError};
use crate::d3d12::command::{Command, CopyLocation, Rect};
use crate::d3d12::descriptor::{depth_stencil_view, render_target_view};
use crate::d3d12::resource::{info_of, ResourceDesc};
use ash::{vk, Device};
use std::collections::{HashMap, HashSet};
use std::mem::Discriminant;

/// `D3D12_RESOURCE_DIMENSION_BUFFER` and `..._TEXTURE2D`.
const BUFFER: u32 = 1;
const TEXTURE2D: u32 = 3;

/// A device-local image standing for one title resource.
pub(super) struct TitleImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    desc: ResourceDesc,
    info: FormatInfo,
}

/// The copyable part of a `TitleImage`, used while recording.
#[derive(Clone, Copy)]
pub(super) struct Img {
    pub(super) image: vk::Image,
    pub(super) info: FormatInfo,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) mips: u32,
    pub(super) layers: u32,
}

impl TitleImage {
    fn img(&self) -> Img {
        Img {
            image: self.image,
            info: self.info,
            width: self.desc.width as u32,
            height: self.desc.height,
            mips: u32::from(self.desc.mip_levels).max(1),
            layers: u32::from(self.desc.depth_or_array_size).max(1),
        }
    }
}

/// A host-visible buffer that lives until the submission using it is done.
struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

/// What a replay has seen so far (for logs and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub lists: u64,
    pub commands: u64,
    pub clears: u64,
    pub copies: u64,
    /// `Draw`, `DrawIndexed` and `ExecuteIndirect` commands seen.
    pub draws: u64,
    /// Draws recorded into Vulkan (the rest are in `draw_skips`, or indirect).
    pub drawn: u64,
    pub draw_skips: super::draw::DrawSkips,
    pub dispatches: u64,
    pub skipped: u64,
}

/// A read-back image: `bytes` are rows of `width * bytes_per_texel`, top row first.
pub struct Readback {
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    pub bytes: Vec<u8>,
}

/// The Vulkan replay of recorded streams, owning the images of the title's resources.
pub struct VkBackend<'g> {
    pub(super) gpu: &'g Gpu,
    images: HashMap<usize, Option<TitleImage>>,
    pub(super) draw: super::draw::DrawState<'g>,
    pub stats: ReplayStats,
    logged: HashSet<Discriminant<Command>>,
    pub(super) swapchains: HashMap<usize, Option<super::Swapchain<'g>>>,
    pub(super) dump: Option<std::path::PathBuf>,
    pub(super) frames: u64,
}

fn range(
    aspect: vk::ImageAspectFlags,
    mip: u32,
    mips: u32,
    layer: u32,
    layers: u32,
) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: aspect,
        base_mip_level: mip,
        level_count: mips,
        base_array_layer: layer,
        layer_count: layers,
    }
}

/// One memory barrier between everything before and after (all stages and accesses).
pub(super) fn global_barrier(d: &Device, cb: vk::CommandBuffer) {
    let access = vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE;
    let b = [vk::MemoryBarrier::default()
        .src_access_mask(access)
        .dst_access_mask(access)];
    // SAFETY: `cb` is recording.
    unsafe {
        d.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &b,
            &[],
            &[],
        )
    };
}

pub(super) fn mip_size(v: u32, mip: u32) -> u32 {
    (v >> mip).max(1)
}

/// Whether one of `rects` (or the absence of any) covers the whole `w` x `h` area.
fn covers(rects: &[Rect], w: u32, h: u32) -> bool {
    rects.is_empty()
        || rects
            .iter()
            .any(|r| r[0] <= 0 && r[1] <= 0 && r[2] >= w as i32 && r[3] >= h as i32)
}

/// The mips and layers a render or depth view names, from its description words
/// (`dimension_at` is where `ViewDimension` sits, `tex2d` and `array` the dimension values).
pub(super) fn view_range(img: Img, desc: Option<[u32; 6]>, depth: bool) -> (u32, u32, u32) {
    let Some(w) = desc else {
        return (0, 0, img.layers);
    };
    let (tex2d, array, base) = if depth { (3, 4, 3) } else { (4, 5, 2) };
    let v = w[1];
    if v == tex2d {
        (w[base].min(img.mips - 1), 0, 1)
    } else if v == array {
        let first = w[base + 1].min(img.layers - 1);
        let n = w[base + 2].min(img.layers - first).max(1);
        (w[base].min(img.mips - 1), first, n)
    } else {
        (0, 0, img.layers)
    }
}

impl<'g> VkBackend<'g> {
    pub fn new(gpu: &'g Gpu) -> Self {
        VkBackend {
            gpu,
            images: HashMap::new(),
            draw: super::draw::DrawState::new(gpu),
            stats: ReplayStats::default(),
            logged: HashSet::new(),
            swapchains: HashMap::new(),
            dump: None,
            frames: 0,
        }
    }

    /// Replay `lists` in order and wait for the GPU to be idle.
    pub fn execute(&mut self, lists: &[Vec<Command>]) {
        let gpu = self.gpu;
        let mut staging = Vec::new();
        let draws = lists
            .iter()
            .flatten()
            .filter(|c| matches!(c, Command::Draw { .. } | Command::DrawIndexed { .. }))
            .count();
        self.draw.begin(draws);
        let r = gpu.submit_once(|d, cb| {
            for list in lists {
                self.stats.lists += 1;
                self.draw.reset_list();
                for c in list {
                    self.stats.commands += 1;
                    self.record(d, cb, c, &mut staging);
                    if matches!(
                        c,
                        Command::ClearRenderTarget { .. }
                            | Command::ClearDepthStencil { .. }
                            | Command::CopyTextureRegion { .. }
                            | Command::CopyResource { .. }
                    ) {
                        global_barrier(d, cb);
                    }
                }
                self.end_rendering(d, cb);
            }
        });
        if let Err(e) = r {
            log_once(format!("replay submission failed: {e}"));
        }
        self.draw.end();
        for s in staging {
            // SAFETY: the submission has completed (or failed before use).
            unsafe {
                gpu.device.destroy_buffer(s.buffer, None);
                gpu.device.free_memory(s.memory, None);
            }
        }
    }

    /// Image of `res`, made (and moved to `GENERAL`) at first use; None when the resource is
    /// not ours, not a 2D texture, or in a format without a mapping.
    /// The colour image of `res` when a replay already made one: image, width, height.
    pub(super) fn colour_image(&self, res: usize) -> Option<(vk::Image, u32, u32)> {
        let t = self.images.get(&res)?.as_ref()?;
        (t.info.aspect == vk::ImageAspectFlags::COLOR).then_some((
            t.image,
            t.desc.width as u32,
            t.desc.height,
        ))
    }

    pub(super) fn image(&mut self, d: &Device, cb: vk::CommandBuffer, res: usize) -> Option<Img> {
        // SAFETY: `res` was recorded by a list method from a live resource of ours.
        let (desc, _) = unsafe { info_of(res as *mut _) }?;
        if let Some(Some(t)) = self.images.get(&res) {
            if t.desc == desc {
                return Some(t.img());
            }
        }
        if let Some(Some(old)) = self.images.remove(&res) {
            self.draw.forget_image(res);
            // SAFETY: the GPU is idle between submissions; this one has not used it yet.
            unsafe { self.destroy(old) };
        }
        let made = self.make_image(d, cb, desc);
        let out = made.as_ref().map(TitleImage::img);
        self.images.insert(res, made);
        out
    }

    /// The description of the image made for `res`, if there is one.
    pub(super) fn image_desc(&self, res: usize) -> Option<ResourceDesc> {
        self.images.get(&res)?.as_ref().map(|t| t.desc)
    }

    fn make_image(
        &self,
        d: &Device,
        cb: vk::CommandBuffer,
        desc: ResourceDesc,
    ) -> Option<TitleImage> {
        if desc.dimension != TEXTURE2D {
            log_once(format!(
                "no image for resource dimension {}",
                desc.dimension
            ));
            return None;
        }
        let Some(info) = format::lookup(desc.format, desc.flags) else {
            log_once(format!("no Vulkan format for DXGI format {}", desc.format));
            return None;
        };
        let (Ok(w), h) = (u32::try_from(desc.width), desc.height) else {
            return None;
        };
        if w == 0 || h == 0 {
            return None;
        }
        let mut usage = vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED;
        if desc.flags & format::FLAG_DEPTH_STENCIL != 0 {
            usage |= vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT;
        } else if desc.flags & format::FLAG_RENDER_TARGET != 0 {
            usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        }
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(info.format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(u32::from(desc.mip_levels).max(1))
            .array_layers(u32::from(desc.depth_or_array_size).max(1))
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let g = self.gpu;
        // SAFETY: objects are destroyed on each failure path.
        unsafe {
            let image = match d.create_image(&ci, None) {
                Ok(i) => i,
                Err(e) => {
                    log_once(format!("vkCreateImage for {desc:?}: {e}"));
                    return None;
                }
            };
            let req = d.get_image_memory_requirements(image);
            let mem = g
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .and_then(|ty| {
                    d.allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(ty),
                        None,
                    )
                    .ok()
                });
            let Some(memory) = mem else {
                log_once(format!("no memory for an image of {desc:?}"));
                d.destroy_image(image, None);
                return None;
            };
            if d.bind_image_memory(image, memory, 0).is_err() {
                d.free_memory(memory, None);
                d.destroy_image(image, None);
                return None;
            }
            let b = [vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range(
                    info.aspect,
                    0,
                    vk::REMAINING_MIP_LEVELS,
                    0,
                    vk::REMAINING_ARRAY_LAYERS,
                ))];
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &b,
            );
            Some(TitleImage {
                image,
                memory,
                desc,
                info,
            })
        }
    }

    /// # Safety
    /// The GPU no longer uses the image.
    unsafe fn destroy(&self, t: TitleImage) {
        self.gpu.device.destroy_image(t.image, None);
        self.gpu.device.free_memory(t.memory, None);
    }

    /// Log (once per kind of command) that a command is not replayed, and count it.
    fn skip(&mut self, c: &Command, why: &str) {
        self.stats.skipped += 1;
        if self.logged.insert(std::mem::discriminant(c)) {
            let text = format!("{c:?}");
            let name = text.split([' ', '(', '{']).next().unwrap_or("?");
            log_once(format!("replay skips {name}: {why}"));
        }
    }

    fn staging(&self, data: &[u8], staging: &mut Vec<Staging>) -> Result<vk::Buffer> {
        let g = self.gpu;
        let d = &g.device;
        // SAFETY: handles are destroyed in `execute` or right here on failure.
        unsafe {
            let buffer = vk_try(
                "vkCreateBuffer",
                d.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(data.len().max(1) as u64)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                    None,
                ),
            )?;
            let req = d.get_buffer_memory_requirements(buffer);
            let flags =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let Some(ty) = g.memory_type(req.memory_type_bits, flags) else {
                d.destroy_buffer(buffer, None);
                return Err(VkError::NoMemoryType);
            };
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ty),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(VkError::Vk("vkAllocateMemory", e));
                }
            };
            let filled = d
                .bind_buffer_memory(buffer, memory, 0)
                .and_then(|_| d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()))
                .map(|p| {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                    d.unmap_memory(memory);
                });
            if let Err(e) = filled {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
                return Err(VkError::Vk("staging", e));
            }
            staging.push(Staging { buffer, memory });
            Ok(buffer)
        }
    }

    fn record(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        c: &Command,
        staging: &mut Vec<Staging>,
    ) {
        use Command as C;
        self.draw.track(c);
        // Rendering stays open over draws and state changes; anything else ends it.
        if !matches!(
            c,
            C::Draw { .. }
                | C::DrawIndexed { .. }
                | C::SetPipeline { .. }
                | C::ClearState { .. }
                | C::Topology(_)
                | C::Viewports(_)
                | C::Scissors(_)
                | C::BlendFactor(_)
                | C::StencilRef(_)
                | C::DepthBounds { .. }
                | C::IndexBuffer(_)
                | C::VertexBuffers { .. }
                | C::StreamOutput { .. }
                | C::RenderTargets { .. }
                | C::Predication { .. }
                | C::Heaps(_)
                | C::RootSignature { .. }
                | C::RootTable { .. }
                | C::RootConstants { .. }
                | C::RootView { .. }
        ) {
            self.end_rendering(d, cb);
        }
        match c {
            C::Draw { .. } | C::DrawIndexed { .. } => {
                self.stats.draws += 1;
                self.draw_command(d, cb, c);
            }
            C::Dispatch { .. } | C::DispatchMesh { .. } | C::DispatchMeshX { .. } => {
                self.stats.dispatches += 1
            }
            C::ExecuteIndirect { .. } => {
                self.stats.draws += 1;
                self.stats.draw_skips.state += 1;
            }
            C::Barriers(_) | C::BarrierGroups(_) | C::Sync { .. } => global_barrier(d, cb),
            C::ClearRenderTarget {
                handle,
                color,
                rects,
            } => {
                let Some(v) = render_target_view(*handle) else {
                    return self.skip(c, "no render target view at the handle");
                };
                let Some(img) = self.image(d, cb, v.resource) else {
                    return self.skip(c, "the resource has no image");
                };
                let desc = (v.described != 0).then_some(v.desc);
                let (mip, layer, n) = view_range(img, desc, false);
                if !covers(rects, mip_size(img.width, mip), mip_size(img.height, mip)) {
                    return self.skip(c, "partial rectangles");
                }
                // SAFETY: `cb` is recording; the image is in GENERAL.
                unsafe {
                    d.cmd_clear_color_image(
                        cb,
                        img.image,
                        vk::ImageLayout::GENERAL,
                        &vk::ClearColorValue { float32: *color },
                        &[range(vk::ImageAspectFlags::COLOR, mip, 1, layer, n)],
                    )
                };
                self.stats.clears += 1;
            }
            C::ClearDepthStencil {
                handle,
                flags,
                depth,
                stencil,
                rects,
            } => {
                let Some(v) = depth_stencil_view(*handle) else {
                    return self.skip(c, "no depth view at the handle");
                };
                let Some(img) = self.image(d, cb, v.resource) else {
                    return self.skip(c, "the resource has no image");
                };
                let mut aspect = vk::ImageAspectFlags::empty();
                if flags & 1 != 0 {
                    aspect |= vk::ImageAspectFlags::DEPTH;
                }
                if flags & 2 != 0 {
                    aspect |= vk::ImageAspectFlags::STENCIL;
                }
                aspect &= img.info.aspect;
                if aspect.is_empty() {
                    return self.skip(c, "the image has none of the cleared aspects");
                }
                let desc = (v.described != 0).then_some(v.desc);
                let (mip, layer, n) = view_range(img, desc, true);
                if !covers(rects, mip_size(img.width, mip), mip_size(img.height, mip)) {
                    return self.skip(c, "partial rectangles");
                }
                // SAFETY: as above.
                unsafe {
                    d.cmd_clear_depth_stencil_image(
                        cb,
                        img.image,
                        vk::ImageLayout::GENERAL,
                        &vk::ClearDepthStencilValue {
                            depth: *depth,
                            stencil: u32::from(*stencil),
                        },
                        &[range(aspect, mip, 1, layer, n)],
                    )
                };
                self.stats.clears += 1;
            }
            C::CopyBufferRegion {
                dst,
                dst_offset,
                src,
                src_offset,
                len,
            } => {
                // SAFETY: resources recorded from live objects of ours.
                let (a, b) = unsafe { (info_of(*dst as *mut _), info_of(*src as *mut _)) };
                let (Some((da, pa)), Some((db, pb))) = (a, b) else {
                    return self.skip(c, "not our resources");
                };
                let fits = |d: &ResourceDesc, off: u64| {
                    d.dimension == BUFFER && off.checked_add(*len).is_some_and(|e| e <= d.width)
                };
                if !fits(&da, *dst_offset) || !fits(&db, *src_offset) {
                    return self.skip(c, "outside the buffers");
                }
                // SAFETY: both ranges are inside buffers backed by title memory.
                unsafe {
                    std::ptr::copy(
                        (pb + src_offset) as *const u8,
                        (pa + dst_offset) as *mut u8,
                        *len as usize,
                    )
                };
                self.stats.copies += 1;
            }
            C::CopyResource { dst, src, .. } => self.copy_resource(d, cb, c, *dst, *src),
            C::CopyTextureRegion {
                dst,
                x,
                y,
                z,
                src,
                source_box,
                ..
            } => self.copy_texture(d, cb, staging, c, (*dst, *src), (*x, *y, *z), *source_box),
            // State only: nothing to replay without draws.
            C::SetPipeline { .. }
            | C::ClearState { .. }
            | C::Discard { .. }
            | C::Topology(_)
            | C::Viewports(_)
            | C::Scissors(_)
            | C::BlendFactor(_)
            | C::StencilRef(_)
            | C::DepthBounds { .. }
            | C::IndexBuffer(_)
            | C::VertexBuffers { .. }
            | C::StreamOutput { .. }
            | C::RenderTargets { .. }
            | C::Predication { .. }
            | C::Heaps(_)
            | C::RootSignature { .. }
            | C::RootTable { .. }
            | C::RootConstants { .. }
            | C::RootView { .. } => {}
            C::ResolveSubresource { .. }
            | C::CopyMemory { .. }
            | C::FillMemory { .. }
            | C::WriteBottom { .. }
            | C::WaitValue { .. }
            | C::ClearUav { .. }
            | C::BeginQuery { .. }
            | C::EndQuery { .. }
            | C::ResolveQueries { .. } => self.skip(c, "not replayed yet"),
        }
    }

    fn copy_resource(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        c: &Command,
        dst: usize,
        src: usize,
    ) {
        // SAFETY: resources recorded from live objects of ours.
        let (a, b) = unsafe { (info_of(dst as *mut _), info_of(src as *mut _)) };
        let (Some((da, pa)), Some((db, pb))) = (a, b) else {
            return self.skip(c, "not our resources");
        };
        if da.dimension == BUFFER && db.dimension == BUFFER {
            let n = da.width.min(db.width) as usize;
            // SAFETY: both buffers are title memory of at least `n` bytes.
            unsafe { std::ptr::copy(pb as *const u8, pa as *mut u8, n) };
            self.stats.copies += 1;
            return;
        }
        let same = (
            da.dimension,
            da.width,
            da.height,
            da.mip_levels,
            da.depth_or_array_size,
        ) == (
            db.dimension,
            db.width,
            db.height,
            db.mip_levels,
            db.depth_or_array_size,
        ) && format::lookup(da.format, da.flags).map(|f| f.bytes)
            == format::lookup(db.format, db.flags).map(|f| f.bytes);
        if !same {
            return self.skip(c, "different textures");
        }
        let (Some(di), Some(si)) = (self.image(d, cb, dst), self.image(d, cb, src)) else {
            return self.skip(c, "the resource has no image");
        };
        let regions: Vec<vk::ImageCopy> = (0..si.mips)
            .map(|m| {
                let layers = vk::ImageSubresourceLayers {
                    aspect_mask: si.info.aspect & di.info.aspect,
                    mip_level: m,
                    base_array_layer: 0,
                    layer_count: si.layers,
                };
                vk::ImageCopy {
                    src_subresource: layers,
                    dst_subresource: layers,
                    extent: vk::Extent3D {
                        width: mip_size(si.width, m),
                        height: mip_size(si.height, m),
                        depth: 1,
                    },
                    ..Default::default()
                }
            })
            .collect();
        // SAFETY: `cb` is recording; both images are in GENERAL.
        unsafe {
            d.cmd_copy_image(
                cb,
                si.image,
                vk::ImageLayout::GENERAL,
                di.image,
                vk::ImageLayout::GENERAL,
                &regions,
            )
        };
        self.stats.copies += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_texture(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        staging: &mut Vec<Staging>,
        c: &Command,
        (dst, src): (CopyLocation, CopyLocation),
        (x, y, z): (u32, u32, u32),
        source_box: Option<[u32; 6]>,
    ) {
        let CopyLocation::Subresource {
            resource: dres,
            index: dsub,
        } = dst
        else {
            return self.skip(c, "texture to buffer");
        };
        let Some(di) = self.image(d, cb, dres) else {
            return self.skip(c, "the destination has no image");
        };
        let (dmip, dlayer) = (dsub % di.mips, dsub / di.mips);
        if dlayer >= di.layers {
            return self.skip(c, "subresource out of range");
        }
        let layers = |mip, layer| vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: mip,
            base_array_layer: layer,
            layer_count: 1,
        };
        let offset = vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: z as i32,
        };
        match src {
            CopyLocation::Footprint {
                resource,
                offset: boff,
                width,
                height,
                depth,
                row_pitch,
                ..
            } => {
                if di.info.aspect != vk::ImageAspectFlags::COLOR
                    || source_box.is_some_and(|b| b[0] != 0 || b[1] != 0 || b[2] != 0)
                {
                    return self.skip(c, "depth upload or offset source box");
                }
                // SAFETY: a resource recorded from a live object of ours.
                let Some((bd, base)) = (unsafe { info_of(resource as *mut _) }) else {
                    return self.skip(c, "the source is not ours");
                };
                let bpp = di.info.bytes;
                let (w, h) = source_box.map_or((width, height), |b| (b[3], b[4]));
                let fits = x + w <= mip_size(di.width, dmip) && y + h <= mip_size(di.height, dmip);
                if bd.dimension != BUFFER
                    || row_pitch == 0
                    || row_pitch % bpp != 0
                    || w == 0
                    || h == 0
                    || !fits
                    || depth != 1
                {
                    return self.skip(c, "unsupported footprint");
                }
                let len =
                    (u64::from(row_pitch) * u64::from(height)).min(bd.width.saturating_sub(boff));
                if len < u64::from(row_pitch) * u64::from(h - 1) + u64::from(w * bpp) {
                    return self.skip(c, "footprint outside the buffer");
                }
                // SAFETY: the range is inside a buffer backed by title memory.
                let bytes =
                    unsafe { std::slice::from_raw_parts((base + boff) as *const u8, len as usize) };
                let buffer = match self.staging(bytes, staging) {
                    Ok(b) => b,
                    Err(e) => return log_once(format!("staging failed: {e}")),
                };
                let region = [vk::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: row_pitch / bpp,
                    buffer_image_height: height,
                    image_subresource: layers(dmip, dlayer),
                    image_offset: offset,
                    image_extent: vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    },
                }];
                // SAFETY: `cb` is recording; the staging buffer lives until the submit ends.
                unsafe {
                    d.cmd_copy_buffer_to_image(
                        cb,
                        buffer,
                        di.image,
                        vk::ImageLayout::GENERAL,
                        &region,
                    )
                };
                self.stats.copies += 1;
            }
            CopyLocation::Subresource {
                resource: sres,
                index: ssub,
            } => {
                let Some(si) = self.image(d, cb, sres) else {
                    return self.skip(c, "the source has no image");
                };
                let (smip, slayer) = (ssub % si.mips, ssub / si.mips);
                let (sw, sh) = (mip_size(si.width, smip), mip_size(si.height, smip));
                let b = source_box.unwrap_or([0, 0, 0, sw, sh, 1]);
                let (w, h) = (b[3].saturating_sub(b[0]), b[4].saturating_sub(b[1]));
                let fits = b[3] <= sw
                    && b[4] <= sh
                    && x + w <= mip_size(di.width, dmip)
                    && y + h <= mip_size(di.height, dmip);
                if slayer >= si.layers
                    || si.info.bytes != di.info.bytes
                    || si.info.aspect != di.info.aspect
                    || w == 0
                    || h == 0
                    || !fits
                {
                    return self.skip(c, "unsupported texture copy");
                }
                let aspect = di.info.aspect;
                let sub = |mip, layer| vk::ImageSubresourceLayers {
                    aspect_mask: aspect,
                    ..layers(mip, layer)
                };
                let region = [vk::ImageCopy {
                    src_subresource: sub(smip, slayer),
                    src_offset: vk::Offset3D {
                        x: b[0] as i32,
                        y: b[1] as i32,
                        z: 0,
                    },
                    dst_subresource: sub(dmip, dlayer),
                    dst_offset: offset,
                    extent: vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    },
                }];
                // SAFETY: `cb` is recording; both images are in GENERAL.
                unsafe {
                    d.cmd_copy_image(
                        cb,
                        si.image,
                        vk::ImageLayout::GENERAL,
                        di.image,
                        vk::ImageLayout::GENERAL,
                        &region,
                    )
                };
                self.stats.copies += 1;
            }
            CopyLocation::Other { .. } => self.skip(c, "unknown source location"),
        }
    }

    /// Mip 0, layer 0 of the colour image made for `resource`, read back to the CPU. None when
    /// the resource has no colour image yet.
    pub fn read_back(&mut self, resource: usize) -> Option<Readback> {
        let t = self.images.get(&resource)?.as_ref()?;
        if t.info.aspect != vk::ImageAspectFlags::COLOR {
            return None;
        }
        let g = self.gpu;
        let (w, h, bytes) = (t.desc.width as u32, t.desc.height, t.info.bytes);
        let size = u64::from(w) * u64::from(h) * u64::from(bytes);
        let image = t.image;
        let d = &g.device;
        // SAFETY: the buffer and its memory are destroyed before returning; the copy has
        // completed when `submit_once` returns.
        unsafe {
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    None,
                )
                .ok()?;
            let req = d.get_buffer_memory_requirements(buffer);
            let flags =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let memory = g.memory_type(req.memory_type_bits, flags).and_then(|ty| {
                d.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty),
                    None,
                )
                .ok()
            });
            let Some(memory) = memory else {
                d.destroy_buffer(buffer, None);
                return None;
            };
            let mut out = None;
            if d.bind_buffer_memory(buffer, memory, 0).is_ok() {
                let region = [vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    },
                    ..Default::default()
                }];
                let done = g.submit_once(|d, cb| {
                    d.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::GENERAL, buffer, &region)
                });
                if done.is_ok() {
                    if let Ok(p) = d.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) {
                        let mut v = vec![0u8; size as usize];
                        std::ptr::copy_nonoverlapping(p.cast::<u8>(), v.as_mut_ptr(), v.len());
                        d.unmap_memory(memory);
                        out = Some(Readback {
                            width: w,
                            height: h,
                            format: t.info.format,
                            bytes: v,
                        });
                    }
                }
            }
            d.destroy_buffer(buffer, None);
            d.free_memory(memory, None);
            out
        }
    }
}

impl Drop for VkBackend<'_> {
    fn drop(&mut self) {
        // SAFETY: the GPU is idle after the wait; images are ours.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            self.swapchains.clear();
            for (_, t) in std::mem::take(&mut self.images) {
                if let Some(t) = t {
                    self.destroy(t);
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::gpu_or_skip;
    use super::*;
    use crate::d3d12::descriptor::{create_depth_stencil_view, create_render_target_view};
    use crate::d3d12::resource::create_at_address;
    use crate::xobj::IID_IUNKNOWN;
    use std::ffi::c_void;

    /// A texture resource over `mem` (the address is never read for textures).
    pub(in crate::backend::vk) fn texture(w: u64, h: u32, format: u32, flags: u32) -> usize {
        let desc = ResourceDesc {
            dimension: TEXTURE2D,
            alignment: 0,
            width: w,
            height: h,
            depth_or_array_size: 1,
            mip_levels: 1,
            format,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags,
        };
        let mut out: *mut c_void = std::ptr::null_mut();
        // SAFETY: `desc` and `out` are valid for the call.
        let hr = unsafe {
            create_at_address(
                std::ptr::null_mut(),
                0x1000,
                &desc,
                0,
                std::ptr::null(),
                &IID_IUNKNOWN,
                &mut out,
            )
        };
        assert_eq!(hr, 0);
        out as usize
    }

    pub(in crate::backend::vk) fn buffer(mem: &mut [u8]) -> usize {
        let desc = ResourceDesc {
            dimension: BUFFER,
            width: mem.len() as u64,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            ..Default::default()
        };
        let mut out: *mut c_void = std::ptr::null_mut();
        // SAFETY: as above.
        let hr = unsafe {
            create_at_address(
                std::ptr::null_mut(),
                mem.as_mut_ptr() as u64,
                &desc,
                0,
                std::ptr::null(),
                &IID_IUNKNOWN,
                &mut out,
            )
        };
        assert_eq!(hr, 0);
        out as usize
    }

    /// An RTV slot for `res`; the slot's memory is leaked with the test.
    pub(in crate::backend::vk) fn rtv(res: usize) -> u64 {
        let slot = Box::leak(Box::new([0u8; 128])).as_mut_ptr() as u64;
        // SAFETY: a live resource and a 128-byte slot.
        unsafe {
            create_render_target_view(std::ptr::null_mut(), res as *mut _, std::ptr::null(), slot)
        };
        slot
    }

    pub(in crate::backend::vk) fn dsv(res: usize) -> u64 {
        let slot = Box::leak(Box::new([0u8; 128])).as_mut_ptr() as u64;
        // SAFETY: as above.
        unsafe {
            create_depth_stencil_view(std::ptr::null_mut(), res as *mut _, std::ptr::null(), slot)
        };
        slot
    }

    #[test]
    fn clear_then_read_back() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let res = texture(16, 8, 28, 1);
        let handle = rtv(res);
        let clear = |color| Command::ClearRenderTarget {
            handle,
            color,
            rects: vec![],
        };
        b.execute(&[vec![
            Command::Draw {
                vertices: 3,
                instances: 1,
                first_vertex: 0,
                first_instance: 0,
            },
            clear([1.0, 0.0, 0.2, 1.0]),
            Command::Barriers(vec![]),
        ]]);
        assert_eq!(b.stats.clears, 1);
        assert_eq!(b.stats.draws, 1);
        let r = b.read_back(res).unwrap();
        assert_eq!((r.width, r.height), (16, 8));
        assert!(r.bytes.chunks(4).all(|p| p == [255, 0, 51, 255]));
        // A second clear on the same image replaces the contents.
        b.execute(&[vec![clear([0.0, 1.0, 0.0, 1.0])]]);
        let r = b.read_back(res).unwrap();
        assert!(r.bytes.chunks(4).all(|p| p == [0, 255, 0, 255]));
    }

    #[test]
    fn partial_rectangles_are_skipped() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let res = texture(16, 8, 28, 1);
        b.execute(&[vec![Command::ClearRenderTarget {
            handle: rtv(res),
            color: [1.0; 4],
            rects: vec![[0, 0, 4, 4]],
        }]]);
        assert_eq!((b.stats.clears, b.stats.skipped), (0, 1));
    }

    #[test]
    fn depth_clear_and_unknown_view() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let res = texture(8, 8, 40, 2);
        b.execute(&[vec![
            Command::ClearDepthStencil {
                handle: dsv(res),
                flags: 1,
                depth: 0.5,
                stencil: 0,
                rects: vec![],
            },
            Command::ClearRenderTarget {
                handle: Box::leak(Box::new([0u8; 128])).as_mut_ptr() as u64,
                color: [0.0; 4],
                rects: vec![],
            },
        ]]);
        assert_eq!((b.stats.clears, b.stats.skipped), (1, 1));
    }

    #[test]
    fn buffer_upload_copy_and_texture_copy() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        // 4x2 texels, row pitch 32 bytes (8 texels) in the buffer.
        let mut mem = vec![0u8; 64];
        for r in 0..2usize {
            for t in 0..4usize {
                mem[r * 32 + t * 4..][..4].copy_from_slice(&[r as u8 + 1, t as u8, 7, 255]);
            }
        }
        let src_buf = buffer(&mut mem);
        let mut mem2 = vec![0u8; 64];
        let dst_buf = buffer(&mut mem2);
        let tex = texture(4, 2, 28, 0);
        let copy = tex_copy(tex, src_buf);
        let tex2 = texture(4, 2, 28, 0);
        b.execute(&[vec![
            Command::CopyBufferRegion {
                dst: dst_buf,
                dst_offset: 0,
                src: src_buf,
                src_offset: 0,
                len: 64,
            },
            copy,
            Command::CopyResource {
                dst: tex2,
                src: tex,
                flags: 0,
            },
        ]]);
        assert_eq!(mem2, mem);
        assert_eq!(b.stats.copies, 3);
        for t in [tex, tex2] {
            let r = b.read_back(t).unwrap();
            assert_eq!(r.bytes.len(), 32);
            assert_eq!(&r.bytes[0..4], &[1, 0, 7, 255]);
            assert_eq!(&r.bytes[16..20], &[2, 0, 7, 255]);
            assert_eq!(&r.bytes[28..32], &[2, 3, 7, 255]);
        }
    }

    #[test]
    fn present_dumps_a_png() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let dir = std::env::temp_dir().join(format!("nativ64-dump-{}", std::process::id()));
        b.set_dump_dir(dir.clone());
        let res = texture(6, 4, 28, 1);
        b.execute(&[vec![Command::ClearRenderTarget {
            handle: rtv(res),
            color: [0.0, 0.0, 1.0, 1.0],
            rects: vec![],
        }]]);
        b.present(res, None, None);
        b.present(res, Some([0, 0, 2, 2]), None);
        assert_eq!(b.frames(), 2);
        let file = std::fs::File::open(dir.join("frame_00001.png")).unwrap();
        let mut reader = png::Decoder::new(std::io::BufReader::new(file))
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (6, 4));
        assert!(buf[..96].chunks(4).all(|p| p == [0, 0, 255, 255]));
        assert!(dir.join("frame_00002.png").exists());
        std::fs::remove_dir_all(dir).unwrap();
        // A resource nothing rendered to presents nothing and dumps nothing.
        let n = b.frames();
        b.present(texture(4, 4, 28, 1), None, None);
        assert_eq!(b.frames(), n + 1);
    }

    fn rss_kb() -> u64 {
        let s = std::fs::read_to_string("/proc/self/statm").unwrap();
        s.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() * 4
    }

    /// Frames of a title: clear a 4K back buffer, present it. The resident size must not grow
    /// with the frame count (images are reused, per-frame buffers freed).
    #[test]
    fn long_run_keeps_a_stable_footprint() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let dir = std::env::temp_dir().join(format!("nativ64-rss-{}", std::process::id()));
        b.set_dump_dir(dir.clone());
        let back = [texture(3840, 2160, 28, 1), texture(3840, 2160, 28, 1)];
        let views = back.map(rtv);
        let mut frame = |i: usize| {
            b.execute(&[vec![Command::ClearRenderTarget {
                handle: views[i % 2],
                color: [0.0; 4],
                rects: vec![],
            }]]);
            b.present(back[i % 2], None, None);
        };
        for i in 0..8 {
            frame(i);
        }
        let before = rss_kb();
        for i in 8..136 {
            frame(i);
        }
        let grown = rss_kb().saturating_sub(before);
        let _ = std::fs::remove_dir_all(dir);
        assert!(
            grown < 200_000,
            "resident size grew by {grown} kB over 128 frames"
        );
    }

    fn tex_copy(tex: usize, buf: usize) -> Command {
        Command::CopyTextureRegion {
            dst: CopyLocation::Subresource {
                resource: tex,
                index: 0,
            },
            x: 0,
            y: 0,
            z: 0,
            src: CopyLocation::Footprint {
                resource: buf,
                offset: 0,
                format: 28,
                width: 4,
                height: 2,
                depth: 1,
                row_pitch: 32,
            },
            source_box: None,
            flags: 0,
        }
    }
}
