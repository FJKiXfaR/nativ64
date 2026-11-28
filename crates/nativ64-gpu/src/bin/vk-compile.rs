//! `NATIV64_DUMP_SPIRV` helper: compile a dumped vertex+fragment pair as a graphics pipeline,
//! so a driver compiler crash can be reproduced and bisected outside the title.
//!
//!   vk-compile <vert.spv> <frag.spv>
//!
//! Exits 0 when the pipeline was made, 2 when Vulkan refused it, and the process dies with
//! the driver's crash when the compiler does (run one file per process).
use ash::vk;

fn read(path: &str) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(vpath), Some(fpath)) = (args.first(), args.get(1)) else {
        eprintln!("usage: vk-compile <vert.spv> <frag.spv>");
        std::process::exit(64);
    };
    let entry = unsafe { ash::Entry::load() }.expect("the Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let ii = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = unsafe { entry.create_instance(&ii, None) }.expect("an instance");
    // the card the title runs on: the discrete one (or NATIV64_GPU= a name part)
    let want = std::env::var("NATIV64_GPU")
        .unwrap_or_default()
        .to_lowercase();
    let mut phys = unsafe { instance.enumerate_physical_devices() }.expect("devices");
    phys.sort_by_key(|&p| {
        let q = unsafe { instance.get_physical_device_properties(p) };
        let name = q
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let named = !want.is_empty() && name.to_lowercase().contains(&want);
        let discrete = q.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
        (!named, !discrete)
    });
    let phys = *phys.first().expect("a device");
    let props = unsafe { instance.get_physical_device_properties(phys) };
    let name = props
        .device_name_as_c_str()
        .unwrap_or(CStr::from_bytes_with_nul(b"?\0").unwrap());
    eprintln!("device: {}", name.to_string_lossy());
    let queue = unsafe { instance.get_physical_device_queue_family_properties(phys) }
        .iter()
        .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
        .expect("a compute queue") as u32;
    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue)
        .queue_priorities(&prio)];
    let core = vk::PhysicalDeviceFeatures::default().shader_int64(true);
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
        .buffer_device_address(true)
        .descriptor_indexing(true)
        .runtime_descriptor_array(true)
        .descriptor_binding_partially_bound(true)
        .shader_sampled_image_array_non_uniform_indexing(true)
        .shader_storage_image_array_non_uniform_indexing(true)
        .shader_storage_buffer_array_non_uniform_indexing(true);
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
    let has_mesh = unsafe { instance.enumerate_device_extension_properties(phys) }
        .unwrap_or_default()
        .iter()
        .any(|e| e.extension_name_as_c_str() == Ok(vk::EXT_MESH_SHADER_NAME));
    let mut mesh = vk::PhysicalDeviceMeshShaderFeaturesEXT::default().mesh_shader(has_mesh);
    let mut f2 = vk::PhysicalDeviceFeatures2::default().features(core);
    let di = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qi)
        .push_next(&mut f2)
        .push_next(&mut f12)
        .push_next(&mut f13);
    let exts: Vec<*const std::ffi::c_char> = if has_mesh {
        vec![vk::EXT_MESH_SHADER_NAME.as_ptr()]
    } else {
        Vec::new()
    };
    let mut di = di.enabled_extension_names(&exts);
    if has_mesh {
        di = di.push_next(&mut mesh);
    }
    let device = unsafe { instance.create_device(phys, &di, None) }.expect("a device");
    // the sets our pipeline layout has: 0 = sampled images, storage images, samplers; 1 = static
    // samplers; the root block is an 8-byte push constant
    let bind = |b: u32, ty, n: u32| {
        vk::DescriptorSetLayoutBinding::default()
            .binding(b)
            .descriptor_type(ty)
            .descriptor_count(n)
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
    };
    let s0 = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                bind(0, vk::DescriptorType::SAMPLED_IMAGE, 1),
                bind(1, vk::DescriptorType::STORAGE_IMAGE, 1),
                bind(2, vk::DescriptorType::SAMPLER, 1),
            ]),
            None,
        )
    }
    .expect("set 0");
    let s1 = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[bind(
                0,
                vk::DescriptorType::SAMPLER,
                1,
            )]),
            None,
        )
    }
    .expect("set 1");
    let sets = [s0, s1];
    let pc = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(8)];
    let layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&sets)
                .push_constant_ranges(&pc),
            None,
        )
    }
    .expect("layout");
    let module = |words: &[u32]| unsafe {
        device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
            .expect("a module")
    };
    let mesh_stage = std::env::var_os("VK_COMPILE_MESH").is_some();
    let (vert, frag) = (module(&read(vpath)), module(&read(fpath)));
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(if mesh_stage {
                vk::ShaderStageFlags::MESH_EXT
            } else {
                vk::ShaderStageFlags::VERTEX
            })
            .module(vert)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(c"main"),
    ];
    let dynamic = vec![
        vk::DynamicState::VIEWPORT,
        vk::DynamicState::SCISSOR,
        vk::DynamicState::PRIMITIVE_TOPOLOGY,
    ];
    let dy = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let vi = vk::PipelineVertexInputStateCreateInfo::default();
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rs = vk::PipelineRasterizationStateCreateInfo::default().line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let cb = vk::PipelineColorBlendStateCreateInfo::default();
    let mut rendering = vk::PipelineRenderingCreateInfo::default()
        .color_attachment_formats(&[vk::Format::B8G8R8A8_UNORM]);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vi)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .color_blend_state(&cb)
        .dynamic_state(&dy)
        .layout(layout)
        .push_next(&mut rendering);
    eprintln!("compiling...");
    let made =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
    match made {
        Ok(_) => {
            eprintln!("pipeline made");
            std::process::exit(0);
        }
        Err((_, e)) => {
            eprintln!("refused: {e:?}");
            std::process::exit(2);
        }
    }
}
