//! Instance creation: Vulkan 1.3 when the loader has it, else 1.2; the validation layer only
//! when `NATIV64_VK_VALIDATION=1` and installed.

use super::{log_once, vk_try, Result, VkError};
use ash::{khr, vk, Entry, Instance};
use std::ffi::CStr;

const VALIDATION: &CStr = c"VK_LAYER_KHRONOS_validation";

/// The instance and whether it carries `VK_KHR_surface` + `VK_KHR_wayland_surface`.
pub fn create(entry: &Entry) -> Result<(Instance, bool)> {
    // SAFETY: plain query on a loaded entry.
    let loader_version = match unsafe { entry.try_enumerate_instance_version() } {
        Ok(Some(v)) => v,
        _ => vk::API_VERSION_1_0,
    };
    if loader_version < vk::API_VERSION_1_2 {
        return Err(VkError::NoDevice(
            "the Vulkan loader is older than 1.2".into(),
        ));
    }
    let api = if loader_version >= vk::API_VERSION_1_3 {
        vk::API_VERSION_1_3
    } else {
        vk::API_VERSION_1_2
    };
    let app = vk::ApplicationInfo::default()
        .application_name(c"nativ64")
        .api_version(api);

    let mut layers: Vec<*const std::ffi::c_char> = Vec::new();
    if std::env::var("NATIV64_VK_VALIDATION").as_deref() == Ok("1") {
        // SAFETY: plain query.
        let have = unsafe { entry.enumerate_instance_layer_properties() }.unwrap_or_default();
        if have
            .iter()
            .any(|l| l.layer_name_as_c_str().is_ok_and(|n| n == VALIDATION))
        {
            layers.push(VALIDATION.as_ptr());
        } else {
            log_once("validation requested but VK_LAYER_KHRONOS_validation is missing".into());
        }
    }
    // Window-system extensions when the loader has both (headless machines do without).
    // SAFETY: plain query.
    let have = unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
    let has = |n: &CStr| {
        have.iter()
            .any(|e| e.extension_name_as_c_str().is_ok_and(|x| x == n))
    };
    let wsi = has(khr::surface::NAME) && has(khr::wayland_surface::NAME);
    let mut exts: Vec<*const std::ffi::c_char> = Vec::new();
    if wsi {
        exts.push(khr::surface::NAME.as_ptr());
        exts.push(khr::wayland_surface::NAME.as_ptr());
    }
    let info = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_layer_names(&layers)
        .enabled_extension_names(&exts);
    // SAFETY: `info` and what it points to outlive the call.
    let instance = vk_try("vkCreateInstance", unsafe {
        entry.create_instance(&info, None)
    })?;
    Ok((instance, wsi))
}
