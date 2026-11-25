//! Physical device choice and the `Gpu` (device + one graphics/compute queue).
//!
//! Order: discrete, integrated, virtual, CPU (llvmpipe). `NATIV64_GPU` is an index into the
//! enumeration or a case-insensitive part of the device name. A device must offer Vulkan 1.2,
//! `VK_EXT_external_memory_host`, buffer device address and timeline semaphores.

use super::{instance, log_once, vk_try, Result, VkError};
use ash::{ext::external_memory_host, vk, Device, Entry, Instance};
use std::sync::Mutex;

pub struct Gpu {
    pub(super) device: Device,
    pub(super) ext_host: external_memory_host::Device,
    pub(super) queue: Mutex<vk::Queue>,
    pub(super) queue_family: u32,
    pub(super) mem_props: vk::PhysicalDeviceMemoryProperties,
    pub(super) import_align: usize,
    name: String,
    instance: Instance,
    _entry: Entry,
}

/// The queue family to use on `phys`, or why the device cannot be used.
fn check(instance: &Instance, phys: vk::PhysicalDevice) -> std::result::Result<u32, String> {
    // SAFETY: queries on a physical device of this instance.
    unsafe {
        let p = instance.get_physical_device_properties(phys);
        if p.api_version < vk::API_VERSION_1_2 {
            return Err("Vulkan 1.2 missing".into());
        }
        let exts = instance
            .enumerate_device_extension_properties(phys)
            .unwrap_or_default();
        if !exts
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(external_memory_host::NAME))
        {
            return Err("VK_EXT_external_memory_host missing".into());
        }
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f12);
        instance.get_physical_device_features2(phys, &mut f2);
        if f12.buffer_device_address == 0 {
            return Err("buffer device address missing".into());
        }
        if f12.timeline_semaphore == 0 {
            return Err("timeline semaphores missing".into());
        }
        instance
            .get_physical_device_queue_family_properties(phys)
            .iter()
            .position(|q| {
                q.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
            .map(|i| i as u32)
            .ok_or_else(|| "no graphics+compute queue".into())
    }
}

fn type_rank(t: vk::PhysicalDeviceType) -> u32 {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        _ => 3, // CPU (llvmpipe) and others
    }
}

impl Gpu {
    /// Open the best device, honouring the `NATIV64_GPU` environment variable.
    pub fn open() -> Result<Gpu> {
        Gpu::open_with(std::env::var("NATIV64_GPU").ok().as_deref())
    }

    /// Like `open`, with the override given explicitly (an index or a name part).
    pub fn open_with(over: Option<&str>) -> Result<Gpu> {
        // SAFETY: loads libvulkan.so.1; the entry is kept alive by the Gpu.
        let entry = unsafe { Entry::load() }.map_err(|e| VkError::Load(e.to_string()))?;
        let instance = instance::create(&entry)?;
        match Self::pick_and_create(&instance, over) {
            Ok((device, queue_family, phys, name)) => {
                // SAFETY: valid handles of this instance.
                let (mem_props, import_align, ext_host, queue) = unsafe {
                    let mut hp = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
                    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut hp);
                    instance.get_physical_device_properties2(phys, &mut p2);
                    (
                        instance.get_physical_device_memory_properties(phys),
                        hp.min_imported_host_pointer_alignment as usize,
                        external_memory_host::Device::new(&instance, &device),
                        device.get_device_queue(queue_family, 0),
                    )
                };
                Ok(Gpu {
                    device,
                    ext_host,
                    queue: Mutex::new(queue),
                    queue_family,
                    mem_props,
                    import_align,
                    name,
                    instance,
                    _entry: entry,
                })
            }
            Err(e) => {
                // SAFETY: nothing was created from the instance.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn pick_and_create(
        instance: &Instance,
        over: Option<&str>,
    ) -> Result<(Device, u32, vk::PhysicalDevice, String)> {
        // SAFETY: plain enumeration.
        let devs = vk_try("vkEnumeratePhysicalDevices", unsafe {
            instance.enumerate_physical_devices()
        })?;
        // (enumeration index, device, name, type)
        let mut cands = Vec::new();
        for (i, &d) in devs.iter().enumerate() {
            // SAFETY: valid device.
            let p = unsafe { instance.get_physical_device_properties(d) };
            let name = p
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            cands.push((i, d, name, p.device_type));
        }
        if let Some(o) = over {
            let lo = o.to_lowercase();
            let idx = o.parse::<usize>().ok();
            cands.retain(|(i, _, n, _)| match idx {
                Some(k) => *i == k,
                None => n.to_lowercase().contains(&lo),
            });
            if cands.is_empty() {
                return Err(VkError::NoDevice(format!(
                    "NATIV64_GPU={o} matches no device"
                )));
            }
        }
        cands.sort_by_key(|c| (type_rank(c.3), c.0));
        let mut reasons = Vec::new();
        for (_, d, name, _) in cands {
            match check(instance, d) {
                Ok(family) => {
                    let device = Self::create_device(instance, d, family)?;
                    return Ok((device, family, d, name));
                }
                Err(why) => {
                    log_once(format!("skipping {name}: {why}"));
                    reasons.push(format!("{name}: {why}"));
                }
            }
        }
        Err(VkError::NoDevice(if reasons.is_empty() {
            "no Vulkan device present".into()
        } else {
            reasons.join("; ")
        }))
    }

    fn create_device(instance: &Instance, phys: vk::PhysicalDevice, family: u32) -> Result<Device> {
        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&prio)];
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .buffer_device_address(true)
            .timeline_semaphore(true);
        let exts: [*const std::ffi::c_char; 1] = [external_memory_host::NAME.as_ptr()];
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&exts)
            .push_next(&mut f12);
        // SAFETY: `info` and its pointees outlive the call.
        vk_try("vkCreateDevice", unsafe {
            instance.create_device(phys, &info, None)
        })
    }

    /// Name of the chosen physical device.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `minImportedHostPointerAlignment`: required alignment of imported pointers and lengths.
    pub fn import_alignment(&self) -> usize {
        self.import_align
    }

    /// Record one command buffer with `record`, submit it on the queue and wait on a fence.
    pub fn submit_once<F: FnOnce(&Device, vk::CommandBuffer)>(&self, record: F) -> Result<()> {
        let d = &self.device;
        // SAFETY: every object created here is destroyed before returning; the queue is used
        // under its mutex; the command buffer has finished when the fence has signalled.
        unsafe {
            let pool = vk_try(
                "vkCreateCommandPool",
                d.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::TRANSIENT)
                        .queue_family_index(self.queue_family),
                    None,
                ),
            )?;
            let r = (|| {
                let cb = vk_try(
                    "vkAllocateCommandBuffers",
                    d.allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(pool)
                            .command_buffer_count(1),
                    ),
                )?[0];
                vk_try(
                    "vkBeginCommandBuffer",
                    d.begin_command_buffer(
                        cb,
                        &vk::CommandBufferBeginInfo::default()
                            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    ),
                )?;
                record(d, cb);
                vk_try("vkEndCommandBuffer", d.end_command_buffer(cb))?;
                let fence = vk_try(
                    "vkCreateFence",
                    d.create_fence(&vk::FenceCreateInfo::default(), None),
                )?;
                let cbs = [cb];
                let sub = [vk::SubmitInfo::default().command_buffers(&cbs)];
                let r = {
                    let q = self.queue.lock().unwrap();
                    vk_try("vkQueueSubmit", d.queue_submit(*q, &sub, fence))
                }
                .and_then(|_| {
                    vk_try(
                        "vkWaitForFences",
                        d.wait_for_fences(&[fence], true, u64::MAX),
                    )
                });
                d.destroy_fence(fence, None);
                r
            })();
            d.destroy_command_pool(pool, None);
            r
        }
    }

    /// First memory type allowed by `bits` with all of `flags`.
    pub(super) fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: objects borrowing the Gpu are gone (lifetimes); device before instance.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::gpu_or_skip;
    use super::*;

    #[test]
    fn open_reports_a_name() {
        let Some(g) = gpu_or_skip() else { return };
        println!("device: {}", g.name());
        assert!(!g.name().is_empty());
        assert!(g.import_alignment().is_power_of_two());
    }

    #[test]
    fn override_by_index_and_name() {
        let Some(g) = gpu_or_skip() else { return };
        let name = g.name().to_string();
        drop(g);
        // By name part, in another case.
        let part = name.split_whitespace().next().unwrap().to_uppercase();
        let g2 = Gpu::open_with(Some(&part)).unwrap();
        assert!(g2.name().to_lowercase().contains(&part.to_lowercase()));
        drop(g2);
        // By index: some index works.
        let by_index = (0..8).find_map(|i| Gpu::open_with(Some(&i.to_string())).ok());
        assert!(by_index.is_some());
        // An override matching nothing is an error, not a fallback.
        assert!(matches!(
            Gpu::open_with(Some("no-such-gpu-xyz")),
            Err(VkError::NoDevice(_))
        ));
    }

    #[test]
    fn submit_once_runs_the_recorder() {
        let Some(g) = gpu_or_skip() else { return };
        let mut ran = false;
        g.submit_once(|_, _| ran = true).unwrap();
        assert!(ran);
    }
}
