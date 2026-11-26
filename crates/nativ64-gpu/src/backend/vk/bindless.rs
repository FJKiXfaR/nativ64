//! The bindless descriptor set, the view table and the per-submission arena of root blocks
//! (chunk 10a, `docs/shaders.md`, Chunk 10 design).
//!
//! * `Bindless`: one set layout (set 0: binding 0 sampled images, 1 storage images, 2 samplers,
//!   each `UPDATE_AFTER_BIND | PARTIALLY_BOUND` and sized by what the device allows, at most
//!   `1 << 20`), one pool, one set and the pipeline layout (that set and an 8-byte push constant,
//!   the device address of the draw's root block). 10a writes no image: a stage that reads one is
//!   skipped before it gets here.
//! * `ViewTable`: one 32-byte record per descriptor-registry index (`descriptor::VIEWS`),
//!   `{u32 kind, u32 dxgi format, u64 address, u32 size, u32 stride, u32 0, u32 0}`, in a
//!   host-visible, device-addressable buffer that grows by doubling. Kind 1 is a buffer (the
//!   address already translated to the device); kind 0 is anything else (10b fills images).
//! * `HostBuf`: a persistently mapped, device-addressable buffer (the table and the arena).
//!
//! shortcuts: the view table is host-visible memory only (no staging into device-local
//! memory); a record whose memory does not translate stays kind 0 and is tried again at the
//! next submission.

use super::gpumem::GpuMemory;
use super::{log_once, vk_try, Gpu, Result, VkError};
use crate::d3d12::descriptor::View;
use crate::d3d12::resource::info_of_checked;
use ash::vk;

/// Descriptors per binding the bindless set is sized for, at most.
const MAX_BINDLESS: u32 = 1 << 20;
/// Bytes of one view record.
pub const RECORD: usize = 32;

/// A host-visible buffer with a device address, mapped for its whole life.
pub struct HostBuf<'g> {
    gpu: &'g Gpu,
    pub buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    pub size: usize,
    pub address: vk::DeviceAddress,
}

impl<'g> HostBuf<'g> {
    pub fn new(gpu: &'g Gpu, size: usize) -> Result<HostBuf<'g>> {
        let d = &gpu.device;
        let size = size.max(RECORD);
        // SAFETY: each object is destroyed on the error paths and by `Drop`.
        unsafe {
            let buffer = vk_try(
                "vkCreateBuffer",
                d.create_buffer(
                    &vk::BufferCreateInfo::default().size(size as u64).usage(
                        vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                            | vk::BufferUsageFlags::STORAGE_BUFFER,
                    ),
                    None,
                ),
            )?;
            let req = d.get_buffer_memory_requirements(buffer);
            let host =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            // Device-local and host-visible when there is such memory (resizable BAR, integrated
            // GPUs), else plain host memory.
            let ty = gpu
                .memory_type(
                    req.memory_type_bits,
                    host | vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )
                .or_else(|| gpu.memory_type(req.memory_type_bits, host));
            let Some(ty) = ty else {
                d.destroy_buffer(buffer, None);
                return Err(VkError::NoMemoryType);
            };
            let mut flags = vk::MemoryAllocateFlagsInfo::default()
                .flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ty)
                    .push_next(&mut flags),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(VkError::Vk("vkAllocateMemory", e));
                }
            };
            let mapped = d
                .bind_buffer_memory(buffer, memory, 0)
                .and_then(|_| d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()));
            let ptr = match mapped {
                Ok(p) => p.cast::<u8>(),
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(VkError::Vk("vkMapMemory", e));
                }
            };
            std::ptr::write_bytes(ptr, 0, size);
            let address =
                d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer));
            Ok(HostBuf {
                gpu,
                buffer,
                memory,
                ptr,
                size,
                address,
            })
        }
    }

    /// Copies `bytes` to `at` (inside the buffer).
    pub fn write(&mut self, at: usize, bytes: &[u8]) {
        assert!(at + bytes.len() <= self.size);
        // SAFETY: inside the mapping, which lives as long as the buffer.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(at), bytes.len()) };
    }

    /// `n` bytes at `at`, read back.
    pub fn read(&self, at: usize, n: usize) -> Vec<u8> {
        assert!(at + n <= self.size);
        // SAFETY: inside the mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.add(at), n) }.to_vec()
    }
}

impl Drop for HostBuf<'_> {
    fn drop(&mut self) {
        // SAFETY: created by `new`; the queue is idle when a backend drops or replaces it.
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// The bindless set and the pipeline layout every draw uses.
pub struct Bindless<'g> {
    gpu: &'g Gpu,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    pub set: vk::DescriptorSet,
    pub layout: vk::PipelineLayout,
}

impl<'g> Bindless<'g> {
    pub fn new(gpu: &'g Gpu) -> Result<Bindless<'g>> {
        let d = &gpu.device;
        // SAFETY: a query on our device; the objects are destroyed by `Drop` (or here on error).
        unsafe {
            let mut p12 = vk::PhysicalDeviceVulkan12Properties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut p12);
            gpu.instance
                .get_physical_device_properties2(gpu.phys, &mut p2);
            let n = [
                p12.max_per_stage_descriptor_update_after_bind_sampled_images,
                p12.max_per_stage_descriptor_update_after_bind_storage_images,
                p12.max_per_stage_descriptor_update_after_bind_samplers,
                p12.max_descriptor_set_update_after_bind_sampled_images,
                p12.max_descriptor_set_update_after_bind_storage_images,
                p12.max_descriptor_set_update_after_bind_samplers,
            ]
            .into_iter()
            .min()
            .unwrap_or(0)
            .min(MAX_BINDLESS);
            if n == 0 {
                return Err(VkError::Invalid("no update-after-bind descriptors"));
            }
            let stages = vk::ShaderStageFlags::ALL;
            let kinds = [
                vk::DescriptorType::SAMPLED_IMAGE,
                vk::DescriptorType::STORAGE_IMAGE,
                vk::DescriptorType::SAMPLER,
            ];
            let bindings: Vec<_> = kinds
                .iter()
                .enumerate()
                .map(|(i, &t)| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(i as u32)
                        .descriptor_type(t)
                        .descriptor_count(n)
                        .stage_flags(stages)
                })
                .collect();
            let flags = [vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                | vk::DescriptorBindingFlags::PARTIALLY_BOUND; 3];
            let mut bf =
                vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&flags);
            let set_layout = vk_try(
                "vkCreateDescriptorSetLayout",
                d.create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                        .bindings(&bindings)
                        .push_next(&mut bf),
                    None,
                ),
            )?;
            // `this` owns what exists so far: an error returns it and `Drop` cleans up.
            let mut this = Bindless {
                gpu,
                set_layout,
                pool: vk::DescriptorPool::null(),
                set: vk::DescriptorSet::null(),
                layout: vk::PipelineLayout::null(),
            };
            let sizes: Vec<_> = kinds
                .iter()
                .map(|&t| vk::DescriptorPoolSize::default().ty(t).descriptor_count(n))
                .collect();
            this.pool = vk_try(
                "vkCreateDescriptorPool",
                d.create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                ),
            )?;
            let layouts = [set_layout];
            this.set = vk_try(
                "vkAllocateDescriptorSets",
                d.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(this.pool)
                        .set_layouts(&layouts),
                ),
            )?[0];
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(8)];
            this.layout = vk_try(
                "vkCreatePipelineLayout",
                d.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&push),
                    None,
                ),
            )?;
            Ok(this)
        }
    }
}

impl Drop for Bindless<'_> {
    fn drop(&mut self) {
        let d = &self.gpu.device;
        // SAFETY: created by `new` on this device, the queue idle; null handles are no-ops.
        unsafe {
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// What a registry view comes to in the table.
enum Rec {
    /// A buffer view whose memory translated: the record.
    Buffer([u8; RECORD]),
    /// Kind 0: not a buffer (10b handles images and samplers).
    None,
    /// A buffer view whose memory does not translate (yet).
    Retry,
}

/// The record of one registry view: a buffer view of memory that translates is kind 1.
fn record_of(view: &View, mem: &mut GpuMemory<'_>) -> Rec {
    match buffer_view(view) {
        None => Rec::None,
        Some((format, address, size, stride)) => match mem.translate(address, size.max(1)) {
            None => Rec::Retry,
            Some(m) => {
                let mut r = [0u8; RECORD];
                r[0..4].copy_from_slice(&1u32.to_le_bytes());
                r[4..8].copy_from_slice(&format.to_le_bytes());
                r[8..16].copy_from_slice(&m.address.to_le_bytes());
                let size = u32::try_from(size).unwrap_or(u32::MAX);
                r[16..20].copy_from_slice(&size.to_le_bytes());
                r[20..24].copy_from_slice(&stride.to_le_bytes());
                Rec::Buffer(r)
            }
        },
    }
}

/// `(dxgi format, title address, size in bytes, stride)` of a view of a buffer.
fn buffer_view(view: &View) -> Option<(u32, u64, u64, u32)> {
    Some(match view {
        View::Constant(address, size) => (0, *address, u64::from(*size), 0),
        View::Shader(res, desc) => {
            // the registry keeps resource pointers the title created views of, released or not
            let (rd, base) = info_of_checked(*res as *mut _)?;
            if rd.dimension != 1 {
                return None;
            }
            match desc {
                // `{format, dimension (1: buffer), mapping, first element (u64), count, stride,
                // flags (1: raw)}`
                Some(w) if w[1] == 1 => {
                    let (first, count, stride, raw) = (
                        u64::from(w[3]) | u64::from(w[4]) << 32,
                        w[5],
                        w[6],
                        w[7] & 1 != 0,
                    );
                    let elem = if raw { 4 } else { u64::from(stride.max(1)) };
                    (
                        w[0],
                        base + first * elem,
                        u64::from(count) * elem,
                        if raw { 0 } else { stride },
                    )
                }
                Some(_) => return None,
                None => (0, base, rd.width, 0),
            }
        }
        View::Unordered(res, _, desc) => {
            let (rd, base) = info_of_checked(*res as *mut _)?;
            if rd.dimension != 1 {
                return None;
            }
            match desc {
                // `{format, dimension, first element (u64), count, stride, counter offset, flags}`
                Some(w) if w[1] == 1 => {
                    let (first, count, stride, raw) = (
                        u64::from(w[2]) | u64::from(w[3]) << 32,
                        w[4],
                        w[5],
                        w[8] & 1 != 0,
                    );
                    let elem = if raw { 4 } else { u64::from(stride.max(1)) };
                    (
                        w[0],
                        base + first * elem,
                        u64::from(count) * elem,
                        if raw { 0 } else { stride },
                    )
                }
                Some(_) => return None,
                None => (0, base, rd.width, 0),
            }
        }
        _ => return None,
    })
}

/// The view records in a buffer, indexed like the descriptor registry.
pub struct ViewTable<'g> {
    gpu: &'g Gpu,
    buf: HostBuf<'g>,
    /// Records the buffer holds room for.
    capacity: usize,
    /// Registry indices below this have been looked at.
    written: usize,
    /// Indices of buffer views whose memory did not translate: tried again.
    retry: Vec<usize>,
}

impl<'g> ViewTable<'g> {
    pub fn new(gpu: &'g Gpu) -> Result<ViewTable<'g>> {
        let capacity = 1024;
        Ok(ViewTable {
            gpu,
            buf: HostBuf::new(gpu, capacity * RECORD)?,
            capacity,
            written: 0,
            retry: Vec::new(),
        })
    }

    /// The device address of record 0.
    pub fn address(&self) -> vk::DeviceAddress {
        self.buf.address
    }

    /// Forget what was written: addresses changed under the records.
    pub fn reset(&mut self) {
        self.written = 0;
        self.retry.clear();
    }

    fn grow(&mut self, need: usize) -> Result<()> {
        if need <= self.capacity {
            return Ok(());
        }
        let mut capacity = self.capacity;
        while capacity < need {
            capacity *= 2;
        }
        let mut new = HostBuf::new(self.gpu, capacity * RECORD)?;
        let old = self.buf.read(0, self.capacity * RECORD);
        new.write(0, &old);
        self.buf = new;
        self.capacity = capacity;
        Ok(())
    }

    /// Writes the records of the views registered since the last call, and retries the ones
    /// whose memory did not translate. The queue is idle.
    pub fn update(&mut self, mem: &mut GpuMemory<'_>) {
        let total = crate::d3d12::descriptor::view_count();
        if self.grow(total).is_err() {
            log_once("view table: cannot grow the buffer".into());
            return;
        }
        let first = self.written;
        // views tried before and not translated, then the new ones
        let again = std::mem::take(&mut self.retry);
        for index in again {
            let v = crate::d3d12::descriptor::views_from(index)
                .into_iter()
                .next();
            match v.as_ref().map_or(Rec::None, |v| record_of(v, mem)) {
                Rec::Buffer(r) => self.buf.write(index * RECORD, &r),
                Rec::Retry => self.retry.push(index),
                Rec::None => {}
            }
        }
        let views = crate::d3d12::descriptor::views_from(first);
        for (k, v) in views.iter().enumerate() {
            let index = first + k;
            match record_of(v, mem) {
                Rec::Buffer(r) => self.buf.write(index * RECORD, &r),
                Rec::Retry => self.retry.push(index),
                Rec::None => {}
            }
        }
        self.written = first + views.len();
    }
}

#[cfg(test)]
mod tests {
    use super::super::draw::tests::title_memory;
    use super::super::replay::tests::{buffer, texture};
    use super::super::tests::gpu_or_skip;
    use super::*;
    use crate::d3d12::descriptor::{create_constant_buffer_view, create_shader_resource_view};

    /// The registry index a view slot holds (its second word).
    fn index_of(slot: &[u8; 64]) -> usize {
        u32::from_le_bytes(slot[4..8].try_into().unwrap()) as usize
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn buffer_views_get_records_with_translated_addresses() {
        let Some(g) = gpu_or_skip() else { return };
        let mut mem = GpuMemory::new(&g);
        let mut table = ViewTable::new(&g).unwrap();
        let base = title_memory(&[0u8; 4096]);
        // SAFETY: committed title memory of 4096 bytes.
        let res = buffer(unsafe { std::slice::from_raw_parts_mut(base as *mut u8, 4096) });
        let (srv, cbv, lost, tex) = (
            Box::leak(Box::new([0u8; 64])),
            Box::leak(Box::new([0u8; 64])),
            Box::leak(Box::new([0u8; 64])),
            Box::leak(Box::new([0u8; 64])),
        );
        let outside = [0u8; 64];
        // SAFETY: valid descriptions and 64-byte slots.
        unsafe {
            // structured: first element 2, 8 elements of 16 bytes
            let desc = [0u32, 1, 0x1688, 2, 0, 8, 16, 0, 0, 0];
            create_shader_resource_view(
                std::ptr::null_mut(),
                res as *mut _,
                desc.as_ptr().cast(),
                srv.as_mut_ptr() as u64,
            );
            let words = |a: u64, size: u32| [a as u32, (a >> 32) as u32, size, 0];
            create_constant_buffer_view(
                std::ptr::null_mut(),
                words(base + 256, 64).as_ptr().cast(),
                cbv.as_mut_ptr() as u64,
            );
            create_constant_buffer_view(
                std::ptr::null_mut(),
                words(outside.as_ptr() as u64, 64).as_ptr().cast(),
                lost.as_mut_ptr() as u64,
            );
            let t = texture(4, 4, 28, 0);
            create_shader_resource_view(
                std::ptr::null_mut(),
                t as *mut _,
                std::ptr::null(),
                tex.as_mut_ptr() as u64,
            );
        }
        table.update(&mut mem);
        let first = mem.translate(base, 1).unwrap().address;
        let r = table.buf.read(index_of(srv) * RECORD, RECORD);
        assert_eq!(u32_at(&r, 0), 1, "a buffer view is kind 1");
        assert_eq!(u64_at(&r, 8), first + 32);
        assert_eq!((u32_at(&r, 16), u32_at(&r, 20)), (128, 16));
        let r = table.buf.read(index_of(cbv) * RECORD, RECORD);
        assert_eq!(
            (u32_at(&r, 0), u64_at(&r, 8), u32_at(&r, 16)),
            (1, first + 256, 64)
        );
        // memory that is not GPU memory and a texture stay kind 0; only the first is retried
        for slot in [&*lost, &*tex] {
            let r = table.buf.read(index_of(slot) * RECORD, RECORD);
            assert_eq!(u32_at(&r, 0), 0);
        }
        // (the registry is global: other tests' views may be in the list too)
        assert!(table.retry.contains(&index_of(lost)));
        assert!(!table.retry.contains(&index_of(tex)));
        // a reset rewrites the records (the addresses may have changed)
        table.reset();
        table.update(&mut mem);
        assert!(table.retry.contains(&index_of(lost)));
        let r = table.buf.read(index_of(srv) * RECORD, RECORD);
        assert_eq!(u64_at(&r, 8), first + 32);
    }
}
