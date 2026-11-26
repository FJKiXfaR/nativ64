//! Textures and samplers of the bindless set (chunk 10b, `docs/shaders.md`): what a shader
//! reads through `textures[index]` (set 0, binding 0) and `samplers[index]` (binding 2), the
//! index being the registry index of the descriptor the title wrote (`descriptor::VIEWS`).
//!
//! At the start of a submission that has draws (`VkBackend::sync_textures`):
//!
//! * the registry entries added since the last time are classified (`Entry`): a shader
//!   resource view of a texture, a placed view (memory with a resource description), a heap
//!   sampler, or anything else (a hole);
//! * every texture a view names gets its image (`VkBackend::image`) and, when the image is
//!   still the host's (no GPU write has touched it), its host memory is hashed and, if the
//!   hash differs from the last upload, detiled (`texture::detile`, every mip and array slice
//!   of the one colour plane) into a staging buffer and copied in;
//! * the descriptors of the new entries are written: a `VkImageView` per texture (cached on
//!   the image by description), a 1x1 zero RGBA8 view for every index that is not a usable
//!   texture, a `VkSampler` per heap sampler, a default sampler elsewhere, so that no shader
//!   reads an unwritten descriptor. When an image is replaced all descriptors are written again.
//!
//! Copies from a host-owned image upload it first (`host_to_image`).
//!
//! shortcuts: every texture with a view is hashed in every submission with draws, sampled or
//! not (VIctory tracks pages with write guards: the upgrade path if the hash shows in a
//! profile); a depth image is viewed as its depth aspect with its own format; planar and
//! multisampled textures and 1D textures have no image; a view type that the shader's variable
//! does not match (a cube read through the 2D variable) cannot be detected here; the address
//! of a placed view is read from words 10 and 11 of its description **[unverified]**.

use super::format::FormatInfo;
use super::replay::{global_barrier, mip_count, stage_bytes, Staging, TitleImage, VkBackend};
use super::sampler::Spec;
use super::{log_once, Gpu};
use crate::d3d12::descriptor::{views_from, View};
use crate::d3d12::resource::{info_of_checked, ResourceDesc};
use crate::texture::detile;
use ash::{vk, Device};
use nativ64::api_ms_win_core_memory as table;
use std::collections::HashSet;

const TEXTURE1D: u32 = 2;
const TEXTURE2D: u32 = 3;
const TEXTURE3D: u32 = 4;

/// Key of a sampled view of an image: view type, format, first mip, mips, first layer, layers
/// and the four swizzles (raw `VkComponentSwizzle`).
pub(super) type TexKey = [u32; 10];

/// A placed view's image: the address and the resource description words.
pub(super) type PlacedKey = (u64, [u32; 14]);

/// What a registry index holds, for the descriptor sets.
#[derive(Clone)]
enum Entry {
    /// Not a usable texture or sampler.
    Hole,
    Texture {
        res: usize,
        desc: Option<[u32; 10]>,
    },
    Placed {
        key: PlacedKey,
        desc: Option<[u32; 10]>,
    },
    Sampler(Spec),
}

struct Hole {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

/// What `sync_textures` keeps between submissions.
#[derive(Default)]
pub(super) struct TexState {
    /// By registry index.
    entries: Vec<Entry>,
    /// Registry indices below this are in `entries`.
    scanned: usize,
    /// Descriptors from this index on are written (again) at the next sync.
    pub(super) dirty_from: usize,
    /// The resources and placed images with a view.
    sources: Vec<usize>,
    source_set: HashSet<usize>,
    placed: Vec<PlacedKey>,
    /// Submissions that synced, to hash a texture once in each.
    pub(super) submission: u64,
    hole: Option<Hole>,
}

impl TexState {
    /// Destroys the hole image; the queue is idle.
    pub(super) fn destroy(&mut self, d: &Device) {
        if let Some(h) = self.hole.take() {
            // SAFETY: made by `hole`; the queue is idle when the backend drops.
            unsafe {
                d.destroy_image_view(h.view, None);
                d.destroy_image(h.image, None);
                d.free_memory(h.memory, None);
            }
        }
    }
}

/// A 64-bit hash of `b` (four multiplicative lanes, 32 bytes a round).
fn hash64(b: &[u8]) -> u64 {
    const K: [u64; 4] = [
        0x9e37_79b1_85eb_ca87,
        0xc2b2_ae3d_27d4_eb4f,
        0x1656_67b1_9e37_79f9,
        0x27d4_eb2f_1656_67c5,
    ];
    let mut lane = [0x243f_6a88_85a3_08d3u64, 1, 2, 3];
    let (chunks, rest) = b.as_chunks::<32>();
    for c in chunks {
        for (i, l) in lane.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[8 * i..8 * i + 8].try_into().unwrap());
            *l = (*l ^ w).wrapping_mul(K[i]).rotate_left(31);
        }
    }
    let mut h = lane[0].rotate_left(1)
        ^ lane[1].rotate_left(7)
        ^ lane[2].rotate_left(12)
        ^ lane[3].rotate_left(18);
    for &x in rest {
        h = (h ^ u64::from(x)).wrapping_mul(K[0]).rotate_left(11);
    }
    h ^ (b.len() as u64).wrapping_mul(K[3])
}

fn components(mapping: u32) -> vk::ComponentMapping {
    let one = |at: u32| match mapping >> at & 7 {
        0 => vk::ComponentSwizzle::R,
        1 => vk::ComponentSwizzle::G,
        2 => vk::ComponentSwizzle::B,
        3 => vk::ComponentSwizzle::A,
        4 => vk::ComponentSwizzle::ZERO,
        5 => vk::ComponentSwizzle::ONE,
        _ => vk::ComponentSwizzle::IDENTITY,
    };
    vk::ComponentMapping {
        r: one(0),
        g: one(3),
        b: one(6),
        a: one(9),
    }
}

/// `D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING`.
const DEFAULT_MAPPING: u32 = 0x1688;

/// The sampled view of `t` that `desc` (a `D3D12_SHADER_RESOURCE_VIEW_DESC` as words; none: the
/// resource's own) describes, made on first use and cached on the image.
fn texture_view(gpu: &Gpu, t: &mut TitleImage, desc: Option<[u32; 10]>) -> Option<vk::ImageView> {
    let volume = t.desc.dimension == TEXTURE3D;
    let all_mips = mip_count(&t.desc);
    let all_layers = if volume {
        1
    } else {
        u32::from(t.desc.depth_or_array_size).max(1)
    };
    let cube_ok = !volume && all_layers % 6 == 0 && u64::from(t.desc.height) == t.desc.width;
    // (type, dxgi format, mapping, first mip, mips, first layer, layers)
    let (ty, dxgi, mapping, mip, mips, first, layers) = match desc {
        None => (
            if volume {
                vk::ImageViewType::TYPE_3D
            } else if all_layers > 1 {
                vk::ImageViewType::TYPE_2D_ARRAY
            } else {
                vk::ImageViewType::TYPE_2D
            },
            0,
            DEFAULT_MAPPING,
            0,
            u32::MAX,
            0,
            all_layers,
        ),
        Some(w) => {
            let (ty, first, layers) = match w[1] {
                4 => (vk::ImageViewType::TYPE_2D, 0, 1),
                5 => (vk::ImageViewType::TYPE_2D_ARRAY, w[5], w[6]),
                8 => (vk::ImageViewType::TYPE_3D, 0, 1),
                9 => (vk::ImageViewType::CUBE, 0, 6),
                10 => (vk::ImageViewType::CUBE_ARRAY, w[5], w[6].saturating_mul(6)),
                d => {
                    log_once(format!("no texture view for view dimension {d}"));
                    return None;
                }
            };
            if (ty == vk::ImageViewType::TYPE_3D) != volume {
                log_once(format!("a view of dimension {} of {:?}", w[1], t.desc));
                return None;
            }
            if matches!(ty, vk::ImageViewType::CUBE | vk::ImageViewType::CUBE_ARRAY) && !cube_ok {
                log_once(format!("a cube view of {:?}", t.desc));
                return None;
            }
            (ty, w[0], w[2], w[3], w[4], first, layers)
        }
    };
    let mip = mip.min(all_mips - 1);
    let mips = if mips == 0 { u32::MAX } else { mips }.min(all_mips - mip);
    let first = first.min(all_layers - 1);
    let layers = layers.min(all_layers - first).max(1);
    let depth = t.info.aspect.contains(vk::ImageAspectFlags::DEPTH);
    let format = if depth || dxgi == 0 {
        t.info.format
    } else {
        match super::format::lookup(dxgi, 0) {
            Some(FormatInfo { format, .. }) => format,
            None => {
                log_once(format!("no Vulkan format for a view in DXGI format {dxgi}"));
                t.info.format
            }
        }
    };
    let swizzle = components(mapping);
    let key: TexKey = [
        ty.as_raw() as u32,
        format.as_raw() as u32,
        mip,
        mips,
        first,
        layers,
        swizzle.r.as_raw() as u32,
        swizzle.g.as_raw() as u32,
        swizzle.b.as_raw() as u32,
        swizzle.a.as_raw() as u32,
    ];
    if let Some(&v) = t.views.get(&key) {
        return Some(v);
    }
    let ci = vk::ImageViewCreateInfo::default()
        .image(t.image)
        .view_type(ty)
        .format(format)
        .components(swizzle)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: if depth {
                vk::ImageAspectFlags::DEPTH
            } else {
                t.info.aspect
            },
            base_mip_level: mip,
            level_count: mips,
            base_array_layer: first,
            layer_count: layers,
        });
    // SAFETY: a valid image of ours; destroyed with it (`VkBackend::destroy`).
    match unsafe { gpu.device.create_image_view(&ci, None) } {
        Ok(v) => {
            t.views.insert(key, v);
            Some(v)
        }
        Err(e) => {
            log_once(format!("vkCreateImageView (sampled) for {:?}: {e}", t.desc));
            None
        }
    }
}

/// Uploads the host memory of `t` when it is the host's and its hash changed (once in each
/// submission): true when a copy was recorded.
fn upload(
    gpu: &Gpu,
    d: &Device,
    cb: vk::CommandBuffer,
    t: &mut TitleImage,
    submission: u64,
    staging: &mut Vec<Staging>,
    stats: &mut super::ReplayStats,
) -> bool {
    if t.gpu_owned || t.seen == submission {
        return false;
    }
    t.seen = submission;
    let Some(layout) = &t.layout else {
        return false;
    };
    if layout.planes.len() != 1 || t.info.aspect != vk::ImageAspectFlags::COLOR {
        log_once(format!(
            "no upload of a planar or depth texture ({:?})",
            t.desc
        ));
        return false;
    }
    let (at, size) = (t.address as usize, layout.size as usize);
    let committed = table::committed_run(at)
        .is_some_and(|(s, e)| s <= at && at.checked_add(size).is_some_and(|end| end <= e));
    if !committed {
        log_once(format!(
            "texture memory {at:#x} (+{size:#x}) is not committed: not uploaded"
        ));
        return false;
    }
    // SAFETY: the range lies in committed, accessible pages of the memory table.
    let data = unsafe { std::slice::from_raw_parts(at as *const u8, size) };
    let hash = hash64(data);
    stats.hashed += size as u64;
    if t.uploaded == Some(hash) {
        return false;
    }
    let volume = t.desc.dimension == TEXTURE3D;
    let layers = if volume {
        1
    } else {
        u32::from(t.desc.depth_or_array_size).max(1)
    };
    let plane = &layout.planes[0];
    let e = plane.element_bytes as usize;
    let mut bytes: Vec<u8> = Vec::new();
    let mut regions = Vec::new();
    for layer in 0..layers {
        for mip in 0..layout.mip_levels {
            let m = &plane.mips[mip as usize];
            let (row, rows) = (m.width as usize * e, m.height as usize);
            let depth = if volume {
                (u32::from(t.desc.depth_or_array_size) >> mip).max(1)
            } else {
                1
            };
            let slice = row * rows;
            let offset = bytes.len().next_multiple_of(16);
            bytes.resize(offset + slice * depth as usize, 0);
            let sub = mip + layer * layout.mip_levels;
            if let Err(err) = detile(
                layout,
                0,
                sub,
                t.address,
                data,
                &mut bytes[offset..],
                (row, slice),
            ) {
                log_once(format!("cannot detile {:?}: {err}", t.desc));
                return false;
            }
            regions.push(vk::BufferImageCopy {
                buffer_offset: offset as u64,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: mip,
                    base_array_layer: layer,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D::default(),
                image_extent: vk::Extent3D {
                    width: super::replay::mip_size(t.desc.width as u32, mip),
                    height: super::replay::mip_size(t.desc.height, mip),
                    depth,
                },
            });
        }
    }
    let buffer = match stage_bytes(gpu, &bytes, staging) {
        Ok(b) => b,
        Err(err) => {
            log_once(format!("staging of a texture upload failed: {err}"));
            return false;
        }
    };
    // SAFETY: `cb` is recording outside a rendering instance; the staging buffer lives until
    // the submission is done; the image is in GENERAL.
    unsafe { d.cmd_copy_buffer_to_image(cb, buffer, t.image, vk::ImageLayout::GENERAL, &regions) };
    t.uploaded = Some(hash);
    t.uploads += 1;
    stats.uploads += 1;
    true
}

fn placed_desc(words: &[u32; 14]) -> ResourceDesc {
    // SAFETY: `ResourceDesc` is 56 bytes of integers (the words are its console form).
    unsafe { std::ptr::read_unaligned(words.as_ptr().cast::<ResourceDesc>()) }
}

impl<'g> VkBackend<'g> {
    /// Ensures the image of `res` exists and holds the host memory's current content when the
    /// host owns it; records no barrier. True when a copy was recorded.
    fn host_upload(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        staging: &mut Vec<Staging>,
        res: usize,
    ) -> bool {
        // a view of a resource the title released stays in the registry
        if info_of_checked(res as *mut _).is_none() || self.image(d, cb, res).is_none() {
            return false;
        }
        let submission = self.tex.submission;
        let Some(Some(mut t)) = self.images.remove(&res) else {
            return false;
        };
        let did = upload(
            self.gpu,
            d,
            cb,
            &mut t,
            submission,
            staging,
            &mut self.stats,
        );
        self.images.insert(res, Some(t));
        did
    }

    /// As `host_upload`, then a barrier: for the source of a copy.
    pub(super) fn host_to_image(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        staging: &mut Vec<Staging>,
        res: usize,
    ) {
        if self.host_upload(d, cb, staging, res) {
            global_barrier(d, cb);
        }
    }

    /// The image of a placed view, made at its first use; None when it has none.
    fn placed_image(&mut self, d: &Device, cb: vk::CommandBuffer, key: PlacedKey) -> bool {
        if let Some(t) = self.placed.get(&key) {
            return t.is_some();
        }
        let desc = placed_desc(&key.1);
        let made = self.make_image(d, cb, desc, key.0);
        if let Some(t) = &made {
            let size = t.layout.as_ref().map_or(1, |l| l.size);
            let (a, b) = (key.0, key.0.saturating_add(size));
            let overlaps = self.images.values().flatten().any(|g| {
                let end = g
                    .address
                    .saturating_add(g.layout.as_ref().map_or(1, |l| l.size));
                g.gpu_owned && g.address < b && a < end
            });
            if overlaps {
                log_once(format!(
                    "a placed view at {a:#x} overlaps an image the GPU wrote: not merged"
                ));
            }
        }
        let ok = made.is_some();
        self.placed.insert(key, made);
        ok
    }

    fn placed_upload(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        staging: &mut Vec<Staging>,
        key: PlacedKey,
    ) -> bool {
        if !self.placed_image(d, cb, key) {
            return false;
        }
        let submission = self.tex.submission;
        let Some(Some(t)) = self.placed.get_mut(&key) else {
            return false;
        };
        upload(self.gpu, d, cb, t, submission, staging, &mut self.stats)
    }

    /// The 1x1 zero RGBA8 view every index that is not a texture holds.
    fn hole(&mut self, d: &Device, cb: vk::CommandBuffer) -> Option<vk::ImageView> {
        if let Some(h) = &self.tex.hole {
            return Some(h.view);
        }
        let g = self.gpu;
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width: 1,
                height: 1,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: objects are destroyed on each failure path or owned by `TexState`.
        unsafe {
            let image = d.create_image(&ci, None).ok()?;
            let req = d.get_image_memory_requirements(image);
            let memory = g
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
            let Some(memory) = memory else {
                d.destroy_image(image, None);
                return None;
            };
            let sub = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let view = d.bind_image_memory(image, memory, 0).ok().and_then(|_| {
                d.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(sub),
                    None,
                )
                .ok()
            });
            let Some(view) = view else {
                d.free_memory(memory, None);
                d.destroy_image(image, None);
                return None;
            };
            let to_general = [vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(sub)];
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_general,
            );
            d.cmd_clear_color_image(
                cb,
                image,
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue { float32: [0.0; 4] },
                &[sub],
            );
            self.tex.hole = Some(Hole {
                image,
                memory,
                view,
            });
            Some(view)
        }
    }

    /// The entry a registry view comes to.
    fn classify(&self, v: &View) -> Entry {
        match v {
            View::Shader(res, desc) => match info_of_checked(*res as *mut _) {
                Some((rd, _)) if rd.dimension == TEXTURE2D || rd.dimension == TEXTURE3D => {
                    Entry::Texture {
                        res: *res,
                        desc: *desc,
                    }
                }
                Some((rd, _)) if rd.dimension == TEXTURE1D => {
                    log_once("a view of a 1D texture is left empty".into());
                    Entry::Hole
                }
                _ => Entry::Hole,
            },
            View::Placed(false, rd, vd) => {
                let desc = placed_desc(rd);
                let address = u64::from(vd[10]) | u64::from(vd[11]) << 32;
                if address == 0 || (desc.dimension != TEXTURE2D && desc.dimension != TEXTURE3D) {
                    return Entry::Hole;
                }
                let mut w = [0u32; 10];
                w.copy_from_slice(&vd[..10]);
                Entry::Placed {
                    key: (address, *rd),
                    desc: (w[1] != 0).then_some(w),
                }
            }
            View::Sampler(_) | View::ConsoleSampler(_) => {
                Spec::of_view(v).map_or(Entry::Hole, Entry::Sampler)
            }
            _ => Entry::Hole,
        }
    }

    /// The first step of a submission with draws: see the module documentation.
    pub(super) fn sync_textures(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        staging: &mut Vec<Staging>,
    ) {
        let Some(count) = self.draw.bindless().map(|b| b.count as usize) else {
            return;
        };
        // 1. the registry entries added since the last time
        let new = views_from(self.tex.scanned);
        for v in &new {
            let entry = self.classify(v);
            match &entry {
                Entry::Texture { res, .. } => {
                    if self.tex.source_set.insert(*res) {
                        self.tex.sources.push(*res);
                    }
                }
                Entry::Placed { key, .. } if !self.tex.placed.contains(key) => {
                    self.tex.placed.push(*key);
                }
                _ => {}
            }
            self.tex.entries.push(entry);
        }
        self.tex.scanned += new.len();
        // 2. images and uploads
        let mut uploaded = false;
        for res in self.tex.sources.clone() {
            uploaded |= self.host_upload(d, cb, staging, res);
        }
        for key in self.tex.placed.clone() {
            uploaded |= self.placed_upload(d, cb, staging, key);
        }
        if uploaded {
            global_barrier(d, cb);
        }
        // 3. descriptors
        let total = self.tex.entries.len();
        let from = self.tex.dirty_from.min(total);
        self.tex.dirty_from = total;
        if from == total {
            return;
        }
        if total > count {
            log_once(format!(
                "{total} descriptors registered, the bindless set holds {count}"
            ));
        }
        let end = total.min(count);
        let hole = self.hole(d, cb);
        let default = self.draw.samplers.get(Spec::default_sampler());
        let mut images = Vec::new();
        let mut samplers = Vec::new();
        for i in from..end {
            let entry = self.tex.entries[i].clone();
            let view = match &entry {
                Entry::Texture { res, desc } => match self.images.get_mut(res) {
                    Some(Some(t)) => texture_view(self.gpu, t, *desc),
                    _ => None,
                },
                Entry::Placed { key, desc } => match self.placed.get_mut(key) {
                    Some(Some(t)) => texture_view(self.gpu, t, *desc),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = view.or(hole) {
                images.push((i, v));
            }
            let sampler = match &entry {
                Entry::Sampler(s) => self.draw.samplers.get(*s).or(default),
                _ => default,
            };
            if let Some(s) = sampler {
                samplers.push((i, s));
            }
        }
        let Some(Some(bl)) = self.draw.bindless.as_ref() else {
            return;
        };
        let set = bl.set;
        let image_infos: Vec<vk::DescriptorImageInfo> = images
            .iter()
            .map(|&(_, v)| {
                vk::DescriptorImageInfo::default()
                    .image_view(v)
                    .image_layout(vk::ImageLayout::GENERAL)
            })
            .collect();
        let sampler_infos: Vec<vk::DescriptorImageInfo> = samplers
            .iter()
            .map(|&(_, s)| vk::DescriptorImageInfo::default().sampler(s))
            .collect();
        let mut writes = Vec::with_capacity(images.len() + samplers.len());
        for (k, &(i, _)) in images.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .dst_array_element(i as u32)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[k..=k]),
            );
        }
        for (k, &(i, _)) in samplers.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(2)
                    .dst_array_element(i as u32)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_infos[k..=k]),
            );
        }
        // SAFETY: the set is update-after-bind and not in use (the queue is idle); the infos
        // outlive the call; the views and samplers are alive.
        unsafe { d.update_descriptor_sets(&writes, &[]) };
    }
}

#[cfg(test)]
mod tests {
    use super::super::draw::tests::{
        clear, draw, floats, pipeline, pixel, state, title_memory, TRIANGLE,
    };
    use super::super::replay::tests::{resource_at, rtv, texture};
    use super::super::tests::gpu_or_skip;
    use super::*;
    use crate::d3d12::command::{Command, CopyLocation, RootView};
    use crate::d3d12::descriptor::{
        create_placed_shader_resource_view, create_sampler, create_shader_resource_view,
    };
    use crate::shader::spirv::tests::stage;
    use crate::texture::{tile, Layout};

    #[test]
    fn the_hash_sees_every_byte() {
        let mut v = vec![7u8; 100];
        let h = hash64(&v);
        assert_eq!(h, hash64(&v.clone()));
        for at in [0, 31, 32, 63, 99] {
            v[at] ^= 1;
            assert_ne!(hash64(&v), h, "byte {at}");
            v[at] ^= 1;
        }
        assert_ne!(hash64(&v[..99]), h);
    }

    #[test]
    fn mappings_follow_d3d12() {
        let m = components(DEFAULT_MAPPING);
        assert_eq!(
            (m.r, m.g, m.b, m.a),
            (
                vk::ComponentSwizzle::R,
                vk::ComponentSwizzle::G,
                vk::ComponentSwizzle::B,
                vk::ComponentSwizzle::A
            )
        );
        // forced zero in red, forced one in alpha, blue from red
        let m = components(4 | 1 << 3 | 5 << 9);
        assert_eq!(
            (m.r, m.g, m.b, m.a),
            (
                vk::ComponentSwizzle::ZERO,
                vk::ComponentSwizzle::G,
                vk::ComponentSwizzle::R,
                vk::ComponentSwizzle::ONE
            )
        );
    }

    fn desc(w: u64, h: u32, mips: u16, format: u32, flags: u32, layout: u32) -> ResourceDesc {
        ResourceDesc {
            dimension: TEXTURE2D,
            width: w,
            height: h,
            depth_or_array_size: 1,
            mip_levels: mips,
            format,
            sample_count: 1,
            layout,
            flags,
            ..Default::default()
        }
    }

    /// Title memory holding the texture `d` in the console layout, each element given by
    /// `texel(mip, layer, x, y)` (`x` and `y` in elements).
    fn tiled_memory(d: &ResourceDesc, texel: impl Fn(u32, u32, u32, u32) -> Vec<u8>) -> u64 {
        let layout = Layout::new(d).unwrap();
        let addr = title_memory(&vec![0u8; layout.size as usize]);
        // SAFETY: committed, writable title memory of `layout.size` bytes.
        let mem = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, layout.size as usize) };
        let p = &layout.planes[0];
        for layer in 0..u32::from(d.depth_or_array_size) {
            for mip in 0..layout.mip_levels {
                let m = &p.mips[mip as usize];
                let e = p.element_bytes as usize;
                let (row, rows) = (m.width as usize * e, m.height as usize);
                let mut linear = vec![0u8; row * rows];
                for y in 0..m.height {
                    for x in 0..m.width {
                        let at = y as usize * row + x as usize * e;
                        linear[at..at + e].copy_from_slice(&texel(mip, layer, x, y));
                    }
                }
                let sub = mip + layer * layout.mip_levels;
                tile(&layout, 0, sub, addr, mem, &linear, (row, row * rows)).unwrap();
            }
        }
        addr
    }

    /// Registers a view at `slot`.
    fn srv(res: usize, view: Option<[u32; 10]>, slot: u64) {
        let p = view.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        // SAFETY: a live resource, a valid description or NULL, and a slot in title memory.
        unsafe { create_shader_resource_view(std::ptr::null_mut(), res as *mut _, p.cast(), slot) };
    }

    /// A heap sampler at `slot`: point filtering, `address` mode U and V, every mip.
    fn sampler(address: u32, slot: u64) {
        let mut w = [0u32; 13];
        w[1..4].copy_from_slice(&[address; 3]);
        w[5] = 1;
        w[6] = 8;
        w[12] = 1000.0f32.to_bits();
        // SAFETY: a valid description and a slot in title memory.
        unsafe { create_sampler(std::ptr::null_mut(), w.as_ptr().cast(), slot) };
    }

    const TARGET: u32 = 1; // allow render target

    /// How many times the image of `res` was uploaded (the registry is global: other tests'
    /// textures are uploaded by this backend too, so the totals in the stats are not ours).
    fn uploads_of(b: &VkBackend, res: usize) -> u32 {
        b.images[&res].as_ref().unwrap().uploads
    }

    /// A table of 256 bytes: the T# at +0x20, the S# at +0x40.
    fn table() -> u64 {
        title_memory(&[0u8; 256])
    }

    /// The commands of one draw of the full-screen triangle with `table` as root parameter 1.
    fn textured_draw(pipe: usize, target: u64, table: u64) -> Vec<Command> {
        let verts = title_memory(&floats(&TRIANGLE));
        let mut l = state(pipe, target);
        l.extend([
            Command::RootView {
                compute: false,
                slot: 0,
                view: RootView::Srv,
                address: verts,
            },
            Command::RootTable {
                compute: false,
                slot: 1,
                handle: table,
            },
            draw(3),
        ]);
        l
    }

    const SAMPLING: fn() -> Vec<u8> = stage::draw_ps_sampling_a_texture;

    fn sampling_pipeline(ps: Vec<u8>) -> usize {
        pipeline(stage::draw_vs_pulling_from_a_root_buffer(), ps, None)
    }

    fn all_pixels(r: &super::super::replay::Readback, want: [u8; 4]) -> bool {
        r.bytes.chunks(4).all(|p| p == want)
    }

    #[test]
    fn a_tiled_texture_with_two_mips_is_detiled_exactly_and_uploaded_when_it_changes() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let d = desc(16, 8, 2, 28, 0, 0);
        let texel = |mip: u32, _: u32, x: u32, y: u32| {
            vec![
                (x * 9 + 1) as u8,
                (y * 17 + 2) as u8,
                (mip * 100 + 3) as u8,
                255,
            ]
        };
        let addr = tiled_memory(&d, texel);
        let tex = resource_at(addr, d);
        let (t0, t1) = (texture(16, 8, 28, TARGET), texture(8, 4, 28, TARGET));
        let copy = |dst, index| Command::CopyTextureRegion {
            dst: CopyLocation::Subresource {
                resource: dst,
                index: 0,
            },
            x: 0,
            y: 0,
            z: 0,
            src: CopyLocation::Subresource {
                resource: tex,
                index,
            },
            source_box: None,
            flags: 0,
        };
        let list = vec![copy(t0, 0), copy(t1, 1)];
        b.execute(std::slice::from_ref(&list));
        assert_eq!(
            (b.stats.copies, uploads_of(&b, tex)),
            (2, 1),
            "{:?}",
            b.stats
        );
        for (res, mip, w, h) in [(t0, 0, 16, 8), (t1, 1, 8, 4)] {
            let r = b.read_back(res).unwrap();
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        pixel(&r, x, y).to_vec(),
                        texel(mip, 0, x as u32, y as u32),
                        "mip {mip} ({x}, {y})"
                    );
                }
            }
        }
        // the same memory again: hashed, not uploaded
        let hashed = b.stats.hashed;
        b.execute(std::slice::from_ref(&list));
        assert_eq!(uploads_of(&b, tex), 1);
        assert!(b.stats.hashed > hashed);
        // changed bytes: uploaded again, and seen
        // SAFETY: committed, writable title memory of the texture.
        unsafe {
            std::ptr::write_bytes(
                addr as *mut u8,
                0xee,
                Layout::new(&d).unwrap().size as usize,
            )
        };
        b.execute(&[list]);
        assert_eq!(uploads_of(&b, tex), 2);
        assert!(b.read_back(t0).unwrap().bytes.contains(&0xee));
    }

    #[test]
    fn a_texture_view_is_sampled_with_its_mapping_and_a_hole_reads_zero() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let d = desc(8, 8, 2, 28, 0, 0);
        let color = |mip: u32| {
            if mip == 0 {
                vec![200, 100, 50, 10]
            } else {
                vec![0, 0, 255, 255]
            }
        };
        let tex = resource_at(tiled_memory(&d, |m, _, _, _| color(m)), d);
        let pipe = sampling_pipeline(SAMPLING());
        let target = texture(16, 8, 28, TARGET);
        let handle = rtv(target);
        let (t1, t2, t3, t4) = (table(), table(), table(), table());
        // the resource's own view, a swizzled one (red from blue, alpha forced to one), a view
        // that is a sampler (not a texture), and a view of mip 1 only
        srv(tex, None, t1 + 0x20);
        let swizzled = [28, 4, 2 | 1 << 3 | 5 << 9, 0, 1, 0, 0, 0, 0, 0];
        srv(tex, Some(swizzled), t2 + 0x20);
        sampler(1, t3 + 0x20);
        let mip1 = [28, 4, 0x1688, 1, 1, 0, 0, 0, 0, 0];
        srv(tex, Some(mip1), t4 + 0x20);
        let mut list = clear(handle, [0.0, 0.0, 1.0, 1.0]);
        let run = |b: &mut VkBackend, table: u64| {
            let mut l = list.clone();
            l.extend(textured_draw(pipe, handle, table));
            b.execute(&[l]);
            b.read_back(target).unwrap()
        };
        assert_eq!(b.stats.drawn, 0);
        let r = run(&mut b, t1);
        assert_eq!(b.stats.drawn, 1, "{:?}", b.stats);
        assert!(all_pixels(&r, [200, 100, 50, 10]), "{:?}", pixel(&r, 3, 3));
        assert!(all_pixels(&run(&mut b, t2), [50, 100, 200, 255]));
        assert!(
            all_pixels(&run(&mut b, t3), [0, 0, 0, 0]),
            "a hole reads zero"
        );
        assert!(all_pixels(&run(&mut b, t4), [0, 0, 255, 255]), "mip 1 only");
        assert_eq!(uploads_of(&b, tex), 1, "uploaded once, used four times");
        list.clear();
    }

    #[test]
    fn a_render_target_drawn_in_one_list_is_sampled_in_the_next_not_overwritten_from_host() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let d = desc(16, 8, 1, 28, TARGET, 0);
        // host memory that holds something else than what the GPU draws
        let size = Layout::new(&d).unwrap().size as usize;
        let x = resource_at(title_memory(&vec![0x55u8; size]), d);
        let (xh, target) = (rtv(x), texture(16, 8, 28, TARGET));
        let handle = rtv(target);
        let pipe = sampling_pipeline(SAMPLING());
        let t = table();
        srv(x, None, t + 0x20);
        let mut first = clear(xh, [1.0, 0.0, 0.0, 1.0]);
        first.extend(textured_draw(pipe, handle, t));
        b.execute(&[first]);
        // (the host memory was uploaded at the start, then the GPU's clear replaced it)
        assert_eq!(uploads_of(&b, x), 1);
        assert!(all_pixels(&b.read_back(target).unwrap(), [255, 0, 0, 255]));
        let mut second = clear(handle, [0.0, 0.0, 1.0, 1.0]);
        second.extend(textured_draw(pipe, handle, t));
        b.execute(&[second]);
        assert_eq!(
            uploads_of(&b, x),
            1,
            "a GPU-owned image is never uploaded again"
        );
        let r = b.read_back(target).unwrap();
        assert!(all_pixels(&r, [255, 0, 0, 255]), "{:?}", pixel(&r, 2, 2));
    }

    #[test]
    fn heap_samplers_clamp_and_wrap() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        // red on the left half, green on the right: (1.0, 0.5) is texel 0 when wrapped and
        // texel 7 when clamped
        let d = desc(8, 8, 1, 28, 0, 0);
        let tex = resource_at(
            tiled_memory(&d, |_, _, x, _| {
                if x < 4 {
                    vec![255, 0, 0, 255]
                } else {
                    vec![0, 255, 0, 255]
                }
            }),
            d,
        );
        let pipe = sampling_pipeline(stage::draw_ps_sampling_a_heap_sampler());
        let (wrap, clamp) = (table(), table());
        for (t, mode) in [(wrap, 1), (clamp, 3)] {
            srv(tex, None, t + 0x20);
            sampler(mode, t + 0x40);
        }
        let target = texture(16, 8, 28, TARGET);
        let mut list = textured_draw(pipe, rtv(target), wrap);
        b.execute(&[std::mem::take(&mut list)]);
        assert!(
            all_pixels(&b.read_back(target).unwrap(), [255, 0, 0, 255]),
            "wrap"
        );
        b.execute(&[textured_draw(pipe, rtv(target), clamp)]);
        assert!(
            all_pixels(&b.read_back(target).unwrap(), [0, 255, 0, 255]),
            "clamp"
        );
    }

    #[test]
    fn a_bc1_texture_and_a_placed_view_are_uploaded_and_sampled() {
        let Some(g) = gpu_or_skip() else { return };
        if !g.caps.bc {
            return;
        }
        let mut b = VkBackend::new(&g);
        // 16 x 8 texels: 4 x 2 blocks; the block holding texel (0, 4) is blue, the rest red
        let d = desc(16, 8, 1, 71, 0, 0);
        let block = |color: u16| {
            let mut v = color.to_le_bytes().to_vec();
            v.extend([0, 0, 0, 0, 0, 0]);
            v
        };
        let tex = resource_at(
            tiled_memory(&d, |_, _, x, y| {
                if (x, y) == (0, 1) {
                    block(0x001f)
                } else {
                    block(0xf800)
                }
            }),
            d,
        );
        let pipe = sampling_pipeline(stage::draw_ps_sampling_a_heap_sampler());
        let t = table();
        srv(tex, None, t + 0x20);
        sampler(1, t + 0x40);
        let target = texture(16, 8, 28, TARGET);
        b.execute(&[textured_draw(pipe, rtv(target), t)]);
        assert_eq!(uploads_of(&b, tex), 1, "{:?}", b.stats);
        assert!(all_pixels(&b.read_back(target).unwrap(), [0, 0, 255, 255]));

        // the same memory through a placed view of an RGBA8 texture
        let pd = desc(4, 4, 1, 28, 0, 0);
        let green = tiled_memory(&pd, |_, _, _, _| vec![0, 255, 0, 255]);
        // SAFETY: `ResourceDesc` is 56 bytes of integers.
        let rd: [u32; 14] = unsafe { std::mem::transmute(pd) };
        let mut vd = [0u32; 16];
        vd[0] = 28;
        vd[1] = 4;
        vd[2] = 0x1688;
        vd[4] = 1;
        vd[10] = green as u32;
        vd[11] = (green >> 32) as u32;
        let t2 = table();
        // SAFETY: valid descriptions and a slot in title memory.
        unsafe {
            create_placed_shader_resource_view(
                std::ptr::null_mut(),
                rd.as_ptr().cast(),
                vd.as_ptr().cast(),
                t2 + 0x20,
            )
        };
        sampler(1, t2 + 0x40);
        b.execute(&[textured_draw(pipe, rtv(target), t2)]);
        assert!(all_pixels(&b.read_back(target).unwrap(), [0, 255, 0, 255]));
        let placed = b.placed.values().flatten().find(|t| t.address == green);
        assert_eq!(placed.map(|t| t.uploads), Some(1));
    }
}
