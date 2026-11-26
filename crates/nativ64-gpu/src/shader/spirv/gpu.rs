//! Runs a probe module on a Vulkan device (tests only, off by default: set
//! `NATIV64_SPIRV_RUN=1`, `NATIV64_SPIRV_DEVICE=<name substring>` picks the device, lavapipe
//! in CI). One dispatch of one invocation per input vector, so the module's results can be
//! compared with the CPU evaluator on the same bytes.

use ash::vk;

pub fn enabled() -> bool {
    std::env::var_os("NATIV64_SPIRV_RUN").is_some()
}

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    mem_type: u32,
}

/// A module's pipeline with its two buffers.
pub struct Run<'a> {
    gpu: &'a Gpu,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    set: vk::DescriptorSet,
    bufs: [(vk::Buffer, vk::DeviceMemory, *mut u32); 2],
    n_in: usize,
    n_out: usize,
}

impl Gpu {
    pub fn new() -> Result<Self, String> {
        let e = |x: vk::Result| format!("{x:?}");
        let entry = unsafe { ash::Entry::load() }.map_err(|e| e.to_string())?;
        let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 2, 0));
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .map_err(e)?;
        let want = std::env::var("NATIV64_SPIRV_DEVICE").unwrap_or_default();
        let devs = unsafe { instance.enumerate_physical_devices() }.map_err(e)?;
        let mut pick = None;
        for d in devs {
            let p = unsafe { instance.get_physical_device_properties(d) };
            let name = p
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().to_string())
                .unwrap_or_default();
            if pick.is_none() && name.contains(&want) {
                pick = Some((d, name));
            }
        }
        let (pd, name) = pick.ok_or("no matching Vulkan device")?;
        eprintln!("spirv probe runs on {name}");
        let q = unsafe { instance.get_physical_device_queue_family_properties(pd) }
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .ok_or("no compute queue")? as u32;
        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(q)
            .queue_priorities(&prio)];
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default().buffer_device_address(true);
        let device = unsafe {
            instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qi)
                    .push_next(&mut f12),
                None,
            )
        }
        .map_err(e)?;
        let queue = unsafe { device.get_device_queue(q, 0) };
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(q),
                None,
            )
        }
        .map_err(e)?;
        let mp = unsafe { instance.get_physical_device_memory_properties(pd) };
        let want_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let mem_type = (0..mp.memory_type_count)
            .find(|&i| {
                mp.memory_types[i as usize]
                    .property_flags
                    .contains(want_flags)
            })
            .ok_or("no host-visible memory")?;
        Ok(Self {
            _entry: entry,
            instance,
            device,
            queue,
            pool,
            mem_type,
        })
    }

    pub fn load(&self, words: &[u32], n_in: usize, n_out: usize) -> Result<Run<'_>, String> {
        let e = |x: vk::Result| format!("{x:?}");
        let d = &self.device;
        unsafe {
            let shader = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
                .map_err(e)?;
            let binds: Vec<_> = (0..2)
                .map(|b| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                })
                .collect();
            let set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
                    None,
                )
                .map_err(e)?;
            let layouts = [set_layout];
            let layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts),
                    None,
                )
                .map_err(e)?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(shader)
                .name(c"main");
            let pipeline = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(layout)],
                    None,
                )
                .map_err(|(_, x)| format!("{x:?}"))?[0];
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(2)];
            let desc_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(e)?;
            let mut bufs = [(
                vk::Buffer::null(),
                vk::DeviceMemory::null(),
                std::ptr::null_mut(),
            ); 2];
            for (k, n) in [n_in.max(1), n_out.max(1)].into_iter().enumerate() {
                let size = 4 * n as u64;
                let b = d
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(size)
                            .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                        None,
                    )
                    .map_err(e)?;
                let req = d.get_buffer_memory_requirements(b);
                let m = d
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(self.mem_type),
                        None,
                    )
                    .map_err(e)?;
                d.bind_buffer_memory(b, m, 0).map_err(e)?;
                let p = d
                    .map_memory(m, 0, size, vk::MemoryMapFlags::empty())
                    .map_err(e)? as *mut u32;
                bufs[k] = (b, m, p);
            }
            let set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(desc_pool)
                        .set_layouts(&layouts),
                )
                .map_err(e)?[0];
            let infos: Vec<[vk::DescriptorBufferInfo; 1]> = bufs
                .iter()
                .map(|b| {
                    [vk::DescriptorBufferInfo::default()
                        .buffer(b.0)
                        .range(vk::WHOLE_SIZE)]
                })
                .collect();
            let writes: Vec<_> = (0..2)
                .map(|k| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(k as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&infos[k])
                })
                .collect();
            d.update_descriptor_sets(&writes, &[]);
            Ok(Run {
                gpu: self,
                layout,
                pipeline,
                set,
                bufs,
                n_in,
                n_out,
            })
        }
    }
}

impl Run<'_> {
    /// One dispatch with `ins` in the first buffer; the second buffer comes back.
    pub fn go(&self, ins: &[u32]) -> Result<Vec<u32>, String> {
        let e = |x: vk::Result| format!("{x:?}");
        let g = self.gpu;
        let d = &g.device;
        unsafe {
            for (k, v) in ins.iter().enumerate().take(self.n_in) {
                *self.bufs[0].2.add(k) = *v;
            }
            for k in 0..self.n_out {
                *self.bufs[1].2.add(k) = 0xdead_beef;
            }
            let cb = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(g.pool)
                        .command_buffer_count(1),
                )
                .map_err(e)?[0];
            d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())
                .map_err(e)?;
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[self.set],
                &[],
            );
            d.cmd_dispatch(cb, 1, 1, 1);
            d.end_command_buffer(cb).map_err(e)?;
            let fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(e)?;
            let cbs = [cb];
            d.queue_submit(
                g.queue,
                &[vk::SubmitInfo::default().command_buffers(&cbs)],
                fence,
            )
            .map_err(e)?;
            d.wait_for_fences(&[fence], true, u64::MAX).map_err(e)?;
            d.destroy_fence(fence, None);
            d.free_command_buffers(g.pool, &cbs);
            Ok((0..self.n_out).map(|k| *self.bufs[1].2.add(k)).collect())
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
