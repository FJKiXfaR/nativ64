//! Physical device choice and the `Gpu` (device + one graphics/compute queue).
//!
//! Order: discrete, integrated, virtual, CPU (llvmpipe). `NATIV64_GPU` is an index into the
//! enumeration or a case-insensitive part of the device name. A device must offer Vulkan 1.3
//! (dynamic rendering, the extended dynamic state of 1.3, demote to helper invocation),
//! `VK_EXT_external_memory_host`, buffer device address, timeline semaphores, 64-bit integers,
//! draw parameters, formatless storage image reads and writes (compute, chunk 10c)
//! and the descriptor indexing the bindless set needs (update after bind,
//! partially bound, runtime arrays, non-uniform indexing of sampled and storage images); the
//! refusal names the first missing feature (chunk 10).

use super::{instance, log_once, vk_try, Result, VkError};
use ash::{ext::external_memory_host, ext::memory_budget, khr, vk, Device, Entry, Instance};
use std::sync::Mutex;

/// `vkGetDeviceFaultInfoEXT` (VK_EXT_device_fault): the fault a lost device reports.
pub(super) type DeviceFaultFn = unsafe extern "system" fn(
    vk::Device,
    *mut vk::DeviceFaultCountsEXT<'_>,
    *mut vk::DeviceFaultInfoEXT<'_>,
) -> vk::Result;

/// `vkCmdDrawMeshTasksEXT`: the mesh stage's draw (VK_EXT_mesh_shader).
pub type MeshDrawFn = unsafe extern "system" fn(vk::CommandBuffer, u32, u32, u32);

/// What `pick_and_create` found: the device and everything opened with it.
type Picked = (
    Device,
    u32,
    vk::PhysicalDevice,
    String,
    bool,
    Option<DeviceFaultFn>,
    Option<MeshDrawFn>,
);

pub struct Gpu {
    pub(super) device: Device,
    /// `vkCmdDrawMeshTasksEXT` when the device has mesh shaders.
    pub(super) mesh_draw: Option<MeshDrawFn>,
    pub(super) ext_host: external_memory_host::Device,
    pub(super) queue: Mutex<vk::Queue>,
    pub(super) queue_family: u32,
    pub(super) mem_props: vk::PhysicalDeviceMemoryProperties,
    pub(super) import_align: usize,
    pub(super) phys: vk::PhysicalDevice,
    /// `VK_KHR_surface` and `VK_KHR_wayland_surface` are enabled on the instance.
    pub(super) wsi_instance: bool,
    /// `VK_KHR_swapchain` is enabled on the device.
    pub(super) wsi_device: bool,
    pub(super) entry: Entry,
    name: String,
    pub(super) instance: Instance,
    /// Sampler and texture features optional in Vulkan (chunk 10b).
    pub(super) caps: Caps,
    /// `VK_EXT_memory_budget` is enabled on the device.
    memory_budget: bool,
    /// Command pool, buffer and fence `submit_once` reuses (created on first use).
    submit: Mutex<Option<SubmitState>>,
    /// `vkGetDeviceFaultInfoEXT` when the device has `VK_EXT_device_fault`.
    device_fault: Option<DeviceFaultFn>,
    /// The driver's pipeline cache (loaded from disk, saved as pipelines are made).
    pub(super) pipeline_cache: vk::PipelineCache,
    /// When the cache was last written out.
    cache_saved: Mutex<Option<std::time::Instant>>,
}

struct SubmitState {
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    fence: vk::Fence,
}

/// Optional features the images and samplers of the replay use when they are there.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Caps {
    /// `samplerAnisotropy`: the largest ratio the device filters with (None: off).
    pub max_anisotropy: Option<f32>,
    /// `samplerMirrorClampToEdge` (core 1.2).
    pub mirror_clamp: bool,
    /// `textureCompressionBC`.
    pub bc: bool,
    /// `geometryShader`: the rectangle lists need it (chunk 10b).
    pub geometry: bool,
    /// `VK_EXT_mesh_shader`: the console's mesh shaders could run as mesh shaders here.
    pub mesh_shader: bool,
    /// `VK_KHR_ray_query` with `VK_KHR_acceleration_structure`: the console's BVH tracing
    /// could become ray queries.
    pub ray_query: bool,
}

/// The features of a device we look at, as the 1.0 struct and the 1.1 to 1.3 ones.
#[derive(Default)]
struct Features {
    core: vk::PhysicalDeviceFeatures,
    f11: vk::PhysicalDeviceVulkan11Features<'static>,
    f12: vk::PhysicalDeviceVulkan12Features<'static>,
    f13: vk::PhysicalDeviceVulkan13Features<'static>,
}

impl Features {
    fn query(instance: &Instance, phys: vk::PhysicalDevice) -> Features {
        let mut f = Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f.f11)
            .push_next(&mut f.f12)
            .push_next(&mut f.f13);
        // SAFETY: query on a physical device of this instance.
        unsafe { instance.get_physical_device_features2(phys, &mut f2) };
        f.core = f2.features;
        f
    }

    /// The name of the first feature we need that is missing.
    fn missing(&self) -> Option<&'static str> {
        let need = [
            (self.core.shader_int64, "shaderInt64"),
            (
                self.core.shader_storage_image_read_without_format,
                "shaderStorageImageReadWithoutFormat",
            ),
            (
                self.core.shader_storage_image_write_without_format,
                "shaderStorageImageWriteWithoutFormat",
            ),
            (self.core.depth_clamp, "depthClamp"),
            (self.core.independent_blend, "independentBlend"),
            (self.f11.shader_draw_parameters, "shaderDrawParameters"),
            (self.f12.buffer_device_address, "buffer device address"),
            (self.f12.timeline_semaphore, "timeline semaphores"),
            (self.f12.descriptor_indexing, "descriptorIndexing"),
            (self.f12.runtime_descriptor_array, "runtimeDescriptorArray"),
            (
                self.f12.descriptor_binding_partially_bound,
                "descriptorBindingPartiallyBound",
            ),
            (
                self.f12.descriptor_binding_sampled_image_update_after_bind,
                "descriptorBindingSampledImageUpdateAfterBind",
            ),
            (
                self.f12.descriptor_binding_storage_image_update_after_bind,
                "descriptorBindingStorageImageUpdateAfterBind",
            ),
            (
                self.f12.shader_sampled_image_array_non_uniform_indexing,
                "shaderSampledImageArrayNonUniformIndexing",
            ),
            (
                self.f12.shader_storage_image_array_non_uniform_indexing,
                "shaderStorageImageArrayNonUniformIndexing",
            ),
            (self.f13.dynamic_rendering, "dynamicRendering"),
            (self.f13.synchronization2, "synchronization2"),
            (
                self.f13.shader_demote_to_helper_invocation,
                "shaderDemoteToHelperInvocation",
            ),
        ];
        need.into_iter().find(|(v, _)| *v == 0).map(|(_, n)| n)
    }
}

/// The queue family to use on `phys`, or why the device cannot be used.
fn check(instance: &Instance, phys: vk::PhysicalDevice) -> std::result::Result<u32, String> {
    // SAFETY: queries on a physical device of this instance.
    unsafe {
        let p = instance.get_physical_device_properties(phys);
        if p.api_version < vk::API_VERSION_1_3 {
            return Err("Vulkan 1.3 missing".into());
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
        if let Some(name) = Features::query(instance, phys).missing() {
            return Err(format!("feature {name} missing"));
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

/// `VK_EXT_shader_stencil_export`, which a pixel stage that writes the stencil reference needs.
fn ext_shader_stencil_export_name() -> &'static std::ffi::CStr {
    c"VK_EXT_shader_stencil_export"
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
    /// Write the pipeline cache out when new pipelines were made (at most once a minute; the
    /// runner can exit through `_exit`, which skips `Drop`).
    pub fn maybe_save_pipeline_cache(&self) {
        if self.pipeline_cache == vk::PipelineCache::null() {
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let made = PIPELINES_MADE.load(Relaxed);
        if made == PIPELINES_SAVED.load(Relaxed) {
            return;
        }
        let mut last = self.cache_saved.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < std::time::Duration::from_secs(60) {
                return;
            }
        }
        let bytes = unsafe { self.device.get_pipeline_cache_data(self.pipeline_cache) };
        let Ok(bytes) = bytes else {
            return;
        };
        let path = pipeline_cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&path, &bytes).is_ok() {
            PIPELINES_SAVED.store(made, Relaxed);
            *last = Some(std::time::Instant::now());
            log_once(format!(
                "pipeline cache: {} pipelines, {} KiB written",
                made,
                bytes.len() / 1024
            ));
        }
    }

    /// The fault a lost device reports (`VK_EXT_device_fault`), logged; the addresses it names
    /// are returned so the caller can say which of the imported ranges they fall in.
    pub fn log_device_fault(&self) -> Vec<u64> {
        let mut named = Vec::new();
        let Some(f) = self.device_fault else {
            return named;
        };
        let mut counts = vk::DeviceFaultCountsEXT::default();
        // SAFETY: the device exists; a lost device answers this query.
        let r = unsafe { f(self.device.handle(), &mut counts, std::ptr::null_mut()) };
        if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
            log_once(format!("vkGetDeviceFaultInfoEXT: {r:?}"));
            return named;
        }
        let naddr = counts.address_info_count as usize;
        let nvendor = counts.vendor_info_count as usize;
        let mut addrs = vec![vk::DeviceFaultAddressInfoEXT::default(); naddr];
        let mut vendors = vec![vk::DeviceFaultVendorInfoEXT::default(); nvendor];
        // the ash builders take a slice of one: build the struct by hand
        let mut info = vk::DeviceFaultInfoEXT {
            s_type: vk::StructureType::DEVICE_FAULT_INFO_EXT,
            p_next: std::ptr::null_mut(),
            description: [0; vk::MAX_DESCRIPTION_SIZE],
            p_address_infos: addrs.as_mut_ptr(),
            p_vendor_infos: vendors.as_mut_ptr(),
            p_vendor_binary_data: std::ptr::null_mut(),
            _marker: std::marker::PhantomData,
        };
        // SAFETY: the arrays outlive the call.
        let r = unsafe { f(self.device.handle(), &mut counts, &mut info) };
        if r != vk::Result::SUCCESS {
            log_once(format!("vkGetDeviceFaultInfoEXT (filled): {r:?}"));
            return named;
        }
        log_once(format!(
            "device fault: {}",
            info.description_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let _ = &named;
        for a in &addrs {
            log_once(format!(
                "device fault address: {:#x} (reported {:#x}, type {:?})",
                a.reported_address, a.address_precision, a.address_type
            ));
            named.push(a.reported_address);
            named.push(a.address_precision);
        }
        named
    }

    /// Open the best device, honouring the `NATIV64_GPU` environment variable.
    pub fn open() -> Result<Gpu> {
        Gpu::open_with(std::env::var("NATIV64_GPU").ok().as_deref())
    }

    /// Like `open`, with the override given explicitly (an index or a name part).
    pub fn open_with(over: Option<&str>) -> Result<Gpu> {
        let _loader = loader_lock();
        let entry = entry()?;
        let (instance, wsi_instance) = instance::create(&entry)?;
        match Self::pick_and_create(&instance, over) {
            Ok((device, queue_family, phys, name, wsi_device, device_fault, mesh_draw)) => {
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
                // SAFETY: valid handles of this instance.
                let (have, limits) = unsafe {
                    (
                        Features::query(&instance, phys),
                        instance.get_physical_device_properties(phys).limits,
                    )
                };
                let exts = unsafe { instance.enumerate_device_extension_properties(phys) }
                    .unwrap_or_default();
                let has = |name: &std::ffi::CStr| {
                    exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
                };
                let caps = Caps {
                    max_anisotropy: (have.core.sampler_anisotropy != 0)
                        .then_some(limits.max_sampler_anisotropy),
                    mirror_clamp: have.f12.sampler_mirror_clamp_to_edge != 0,
                    bc: have.core.texture_compression_bc != 0,
                    geometry: have.core.geometry_shader != 0,
                    mesh_shader: has(vk::EXT_MESH_SHADER_NAME),
                    ray_query: has(vk::KHR_RAY_QUERY_NAME)
                        && has(vk::KHR_ACCELERATION_STRUCTURE_NAME),
                };
                log_once(format!(
                    "device: mesh shaders {}, ray query {}, geometry {}",
                    onoff(caps.mesh_shader),
                    onoff(caps.ray_query),
                    onoff(caps.geometry)
                ));
                // SAFETY: a valid device of this instance.
                let memory_budget = unsafe { instance.enumerate_device_extension_properties(phys) }
                    .unwrap_or_default()
                    .iter()
                    .any(|e| e.extension_name_as_c_str() == Ok(memory_budget::NAME));
                log_once(format!(
                    "device fault reporting: {}",
                    if device_fault.is_some() { "on" } else { "off" }
                ));
                // the driver's pipeline cache, kept between runs: a title that compiles a few
                // thousand pipelines pays for it once
                let (pipeline_cache, cache_note) = load_pipeline_cache(&device);
                log_once(format!("pipeline cache: {cache_note}"));
                Ok(Gpu {
                    mesh_draw,
                    pipeline_cache,
                    cache_saved: Mutex::new(None),
                    caps,
                    device_fault,
                    memory_budget,
                    device,
                    ext_host,
                    queue: Mutex::new(queue),
                    queue_family,
                    submit: Mutex::new(None),
                    mem_props,
                    import_align,
                    phys,
                    wsi_instance,
                    wsi_device,
                    entry,
                    name,
                    instance,
                })
            }
            Err(e) => {
                // SAFETY: nothing was created from the instance.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn pick_and_create(instance: &Instance, over: Option<&str>) -> Result<Picked> {
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
                    let (device, wsi, fault, mesh_draw) = Self::create_device(instance, d, family)?;
                    return Ok((device, family, d, name, wsi, fault, mesh_draw));
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

    /// The device and whether `VK_KHR_swapchain` was enabled on it.
    fn create_device(
        instance: &Instance,
        phys: vk::PhysicalDevice,
        family: u32,
    ) -> Result<(Device, bool, Option<DeviceFaultFn>, Option<MeshDrawFn>)> {
        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&prio)];
        // Everything `missing` checked, plus the optional features the pipelines use when they
        // are there (clip distances, sample shading, wireframe, depth bounds, anisotropy,
        // geometry shaders).
        let have_f = Features::query(instance, phys);
        let core = vk::PhysicalDeviceFeatures::default()
            .shader_int64(true)
            .shader_storage_image_read_without_format(true)
            .shader_storage_image_write_without_format(true)
            .depth_clamp(true)
            .independent_blend(true)
            .shader_clip_distance(have_f.core.shader_clip_distance != 0)
            .sample_rate_shading(have_f.core.sample_rate_shading != 0)
            .fill_mode_non_solid(have_f.core.fill_mode_non_solid != 0)
            .depth_bounds(have_f.core.depth_bounds != 0)
            .sampler_anisotropy(have_f.core.sampler_anisotropy != 0)
            .texture_compression_bc(have_f.core.texture_compression_bc != 0)
            .geometry_shader(have_f.core.geometry_shader != 0);
        let mut f11 = vk::PhysicalDeviceVulkan11Features::default().shader_draw_parameters(true);
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .buffer_device_address(true)
            .timeline_semaphore(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(true)
            .descriptor_binding_partially_bound(true)
            .descriptor_binding_sampled_image_update_after_bind(true)
            .descriptor_binding_storage_image_update_after_bind(true)
            .shader_sampled_image_array_non_uniform_indexing(true)
            .shader_storage_image_array_non_uniform_indexing(true)
            .sampler_mirror_clamp_to_edge(have_f.f12.sampler_mirror_clamp_to_edge != 0);
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true)
            .shader_demote_to_helper_invocation(true);
        let mut f2 = vk::PhysicalDeviceFeatures2::default().features(core);
        // SAFETY: query on a physical device of this instance.
        let have =
            unsafe { instance.enumerate_device_extension_properties(phys) }.unwrap_or_default();
        let swapchain = have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(khr::swapchain::NAME));
        let mut exts: Vec<*const std::ffi::c_char> = vec![external_memory_host::NAME.as_ptr()];
        if swapchain {
            exts.push(khr::swapchain::NAME.as_ptr());
        }
        if have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(ext_shader_stencil_export_name()))
        {
            exts.push(ext_shader_stencil_export_name().as_ptr());
        }
        if have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(memory_budget::NAME))
        {
            exts.push(memory_budget::NAME.as_ptr());
        }
        // float min/max atomics on buffers (VK_EXT_shader_atomic_float2)
        let atomic_float2 = have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(vk::EXT_SHADER_ATOMIC_FLOAT2_NAME));
        if atomic_float2 {
            exts.push(vk::EXT_SHADER_ATOMIC_FLOAT2_NAME.as_ptr());
        }
        let mut atomic_float_features = vk::PhysicalDeviceShaderAtomicFloat2FeaturesEXT::default()
            .shader_buffer_float32_atomic_min_max(true);
        // the console's mesh shaders run as Vulkan mesh shaders (VK_EXT_mesh_shader)
        let mesh_shader = have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(vk::EXT_MESH_SHADER_NAME));
        if mesh_shader {
            exts.push(vk::EXT_MESH_SHADER_NAME.as_ptr());
        }
        let mut mesh_features = vk::PhysicalDeviceMeshShaderFeaturesEXT::default()
            .mesh_shader(true)
            .multiview_mesh_shader(false);
        // a lost device then says what faulted (VK_EXT_device_fault)
        let faults = have
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(vk::EXT_DEVICE_FAULT_NAME));
        if faults {
            exts.push(vk::EXT_DEVICE_FAULT_NAME.as_ptr());
        }
        let mut fault_features = vk::PhysicalDeviceFaultFeaturesEXT::default().device_fault(true);
        let mut info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qi)
            .enabled_extension_names(&exts)
            .push_next(&mut f2)
            .push_next(&mut f11)
            .push_next(&mut f12)
            .push_next(&mut f13);
        if faults {
            info = info.push_next(&mut fault_features);
        }
        if atomic_float2 {
            info = info.push_next(&mut atomic_float_features);
        }
        if mesh_shader {
            info = info.push_next(&mut mesh_features);
        }
        // SAFETY: `info` and its pointees outlive the call.
        let device = vk_try("vkCreateDevice", unsafe {
            instance.create_device(phys, &info, None)
        })?;
        let mesh_draw: Option<MeshDrawFn> = if mesh_shader {
            // SAFETY: the device is live and the extension enabled; the function's signature
            // is the extension's (a command buffer and three group counts).
            let loader = ash::ext::mesh_shader::Device::new(instance, &device);
            Some(loader.fp().cmd_draw_mesh_tasks_ext)
        } else {
            None
        };
        let fault_fn = if faults {
            // SAFETY: the device exists; the name is the extension's one function.
            // the command name, not the extension name
            let f = unsafe {
                instance.get_device_proc_addr(device.handle(), c"vkGetDeviceFaultInfoEXT".as_ptr())
            };
            // SAFETY: the extension's function has this signature.
            f.map(|f| unsafe {
                std::mem::transmute::<unsafe extern "system" fn(), DeviceFaultFn>(f)
            })
        } else {
            None
        };
        Ok((device, swapchain, fault_fn, mesh_draw))
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
    ///
    /// One pool, one command buffer and one fence are created on first use and reset for each
    /// call; the `submit` mutex is held for the whole call, so threads take turns.
    pub fn submit_once<F: FnOnce(&Device, vk::CommandBuffer)>(&self, record: F) -> Result<()> {
        let d = &self.device;
        let mut slot = self.submit.lock().unwrap();
        // SAFETY: the objects are owned by `self.submit` (destroyed in `Drop`), used only under
        // its mutex; the previous command buffer has finished (every call waits on the fence).
        unsafe {
            if slot.is_none() {
                let pool = vk_try(
                    "vkCreateCommandPool",
                    d.create_command_pool(
                        &vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family),
                        None,
                    ),
                )?;
                let made = (|| {
                    let cb = vk_try(
                        "vkAllocateCommandBuffers",
                        d.allocate_command_buffers(
                            &vk::CommandBufferAllocateInfo::default()
                                .command_pool(pool)
                                .command_buffer_count(1),
                        ),
                    )?[0];
                    let fence = vk_try(
                        "vkCreateFence",
                        d.create_fence(&vk::FenceCreateInfo::default(), None),
                    )?;
                    Ok::<_, VkError>((cb, fence))
                })();
                match made {
                    Ok((cb, fence)) => *slot = Some(SubmitState { pool, cb, fence }),
                    Err(e) => {
                        d.destroy_command_pool(pool, None);
                        return Err(e);
                    }
                }
            }
            let st = slot.as_ref().unwrap();
            let (cb, fence) = (st.cb, st.fence);
            vk_try(
                "vkResetCommandPool",
                d.reset_command_pool(st.pool, vk::CommandPoolResetFlags::empty()),
            )?;
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
            let cbs = [cb];
            let sub = [vk::SubmitInfo::default().command_buffers(&cbs)];
            let submitted = {
                let q = self.queue.lock().unwrap();
                vk_try("vkQueueSubmit", d.queue_submit(*q, &sub, fence))
            };
            submitted?;
            let waited = vk_try(
                "vkWaitForFences",
                d.wait_for_fences(&[fence], true, u64::MAX),
            );
            // The fence is unsignalled again for the next call, even after a failed wait.
            vk_try("vkResetFences", d.reset_fences(&[fence]))?;
            waited
        }
    }

    /// Handles of the reused submit objects (None before the first `submit_once`).
    #[cfg(test)]
    fn submit_handles(&self) -> Option<(vk::CommandPool, vk::CommandBuffer, vk::Fence)> {
        let s = self.submit.lock().unwrap();
        s.as_ref().map(|s| (s.pool, s.cb, s.fence))
    }

    /// How many bytes of images the device should keep alive: 80% of the device-local heap
    /// budget `VK_EXT_memory_budget` reports when the device has it, else 60% of the largest
    /// device-local heap.
    pub(super) fn image_budget(&self) -> u64 {
        let p = &self.mem_props;
        let heaps = &p.memory_heaps[..p.memory_heap_count as usize];
        let local = |h: &vk::MemoryHeap| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL);
        let Some((at, largest)) = heaps
            .iter()
            .enumerate()
            .filter(|(_, h)| local(h))
            .max_by_key(|(_, h)| h.size)
        else {
            return u64::MAX;
        };
        if self.memory_budget {
            let mut b = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut b);
            // SAFETY: a valid physical device of this instance, the extension is enabled.
            unsafe {
                self.instance
                    .get_physical_device_memory_properties2(self.phys, &mut p2)
            };
            let budget = b.heap_budget[at];
            if budget > 0 {
                return budget / 5 * 4;
            }
        }
        largest.size / 5 * 3
    }

    /// Whether images of `format` can be storage images (optimal tiling).
    pub(super) fn storage_format(&self, format: vk::Format) -> bool {
        // SAFETY: a query on a physical device of this instance.
        let p = unsafe {
            self.instance
                .get_physical_device_format_properties(self.phys, format)
        };
        p.optimal_tiling_features
            .contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
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
            save_pipeline_cache(&self.device, self.pipeline_cache);
            self.device
                .destroy_pipeline_cache(self.pipeline_cache, None);
            if let Some(st) = self.submit.get_mut().unwrap().take() {
                self.device.destroy_fence(st.fence, None);
                self.device.destroy_command_pool(st.pool, None);
            }
            let _loader = loader_lock();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// How many pipelines have been made this run.
pub(super) static PIPELINES_MADE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// The value of `PIPELINES_MADE` the cache on disk already holds.
static PIPELINES_SAVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `on` / `off` for a capability line.
fn onoff(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

/// Where the pipeline cache lives (`NATIV64_PIPELINE_CACHE`, or
/// `$XDG_CACHE_HOME/nativ64/pipelines.bin`).
fn pipeline_cache_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("NATIV64_PIPELINE_CACHE") {
        return p.into();
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("nativ64").join("pipelines.bin")
}

/// The driver's pipeline cache, seeded with the bytes of an earlier run when there are any.
fn load_pipeline_cache(device: &Device) -> (vk::PipelineCache, String) {
    let path = pipeline_cache_path();
    let data = std::fs::read(&path);
    let (bytes, note) = match &data {
        Ok(d) if !d.is_empty() => (
            Some(&d[..]),
            format!("{} bytes from {}", d.len(), path.display()),
        ),
        _ => (None, format!("empty ({})", path.display())),
    };
    let info = vk::PipelineCacheCreateInfo::default().initial_data(bytes.unwrap_or(&[]));
    // SAFETY: the device exists; the initial data is a cache blob the driver validated itself
    // (an invalid one is rejected and the cache starts empty).
    let cache = match unsafe { device.create_pipeline_cache(&info, None) } {
        Ok(c) => c,
        Err(e) => {
            log_once(format!("pipeline cache: cannot create ({e:?})"));
            vk::PipelineCache::null()
        }
    };
    (cache, note)
}

/// Write the pipeline cache out (a later run reuses the compiled pipelines).
fn save_pipeline_cache(device: &Device, cache: vk::PipelineCache) {
    if cache == vk::PipelineCache::null() {
        return;
    }
    // SAFETY: the device exists and is idle.
    let data = unsafe { device.get_pipeline_cache_data(cache) };
    let Ok(bytes) = data else {
        log_once(format!("pipeline cache: cannot read ({:?})", data.err()));
        return;
    };
    let path = pipeline_cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&path, &bytes) {
        Ok(()) => log_once(format!(
            "pipeline cache: saved {} bytes to {}",
            bytes.len(),
            path.display()
        )),
        Err(e) => log_once(format!("pipeline cache: cannot save ({e})")),
    }
}

/// Held while an instance and its device are created or destroyed. The Vulkan loader loads
/// and unloads the drivers and implicit layers there, and doing so on two threads at once
/// (parallel tests opening and dropping `Gpu`s) made one call a null entry point.
fn loader_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process's libvulkan.so.1, loaded once and never unloaded: a `Gpu` dropped on one
/// thread no longer unloads the loader under another thread that is opening one.
fn entry() -> Result<Entry> {
    static ENTRY: std::sync::OnceLock<std::result::Result<Entry, String>> =
        std::sync::OnceLock::new();
    // SAFETY: loads libvulkan.so.1; the static keeps it loaded for the whole process.
    ENTRY
        .get_or_init(|| unsafe { Entry::load() }.map_err(|e| e.to_string()))
        .clone()
        .map_err(VkError::Load)
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
    fn gpus_open_and_drop_on_several_threads() {
        let Some(g) = gpu_or_skip() else { return };
        drop(g);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..8 {
                        drop(Gpu::open().unwrap());
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
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

    #[test]
    fn submit_once_reuses_its_objects() {
        let Some(g) = gpu_or_skip() else { return };
        let mut n = 0;
        g.submit_once(|_, _| n += 1).unwrap();
        let first = g.submit_handles().unwrap();
        g.submit_once(|_, _| n += 1).unwrap();
        assert_eq!(n, 2);
        assert_eq!(g.submit_handles().unwrap(), first);
    }
}
