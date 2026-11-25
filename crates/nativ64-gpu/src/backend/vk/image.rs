//! A device-local 2D image and the read-back path: clear it on the GPU, copy it into a
//! host-imported buffer, read the bytes from the CPU side.

use super::{vk_try, AlignedBuf, Gpu, Result, VkError};
use ash::vk;

const RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// A device-local RGBA8 image (`R8G8B8A8_UNORM`, transfer source and destination).
pub struct Image<'g> {
    gpu: &'g Gpu,
    pub image: vk::Image,
    memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
}

impl Gpu {
    pub fn create_image_rgba8(&self, width: u32, height: u32) -> Result<Image<'_>> {
        if width == 0 || height == 0 {
            return Err(VkError::Invalid("empty image"));
        }
        let d = &self.device;
        // SAFETY: objects are destroyed on every error path.
        unsafe {
            let image = vk_try(
                "vkCreateImage",
                d.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(
                            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                        )
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                ),
            )?;
            let req = d.get_image_memory_requirements(image);
            let Some(ty) =
                self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            else {
                d.destroy_image(image, None);
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
                    d.destroy_image(image, None);
                    return Err(VkError::Vk("vkAllocateMemory", e));
                }
            };
            if let Err(e) = d.bind_image_memory(image, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_image(image, None);
                return Err(VkError::Vk("vkBindImageMemory", e));
            }
            Ok(Image {
                gpu: self,
                image,
                memory,
                width,
                height,
            })
        }
    }

    /// Clear a new `width` x `height` RGBA8 image to `rgba` and return its bytes (row-major,
    /// 4 per pixel), read back through a host-imported buffer.
    pub fn clear_and_read(&self, width: u32, height: u32, rgba: [f32; 4]) -> Result<Vec<u8>> {
        let img = self.create_image_rgba8(width, height)?;
        let n = width as usize * height as usize * 4;
        let mem = AlignedBuf::new(n, self.import_alignment());
        let hb = self.import_host(mem.as_ptr(), mem.len())?;
        let barrier = |d: &ash::Device,
                       cb: vk::CommandBuffer,
                       old: vk::ImageLayout,
                       new: vk::ImageLayout,
                       src: vk::AccessFlags,
                       dst: vk::AccessFlags| {
            let b = [vk::ImageMemoryBarrier::default()
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img.image)
                .subresource_range(RANGE)];
            // SAFETY: `cb` is recording.
            unsafe {
                d.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &b,
                )
            };
        };
        self.submit_once(|d, cb| {
            use vk::{AccessFlags as A, ImageLayout as L};
            barrier(
                d,
                cb,
                L::UNDEFINED,
                L::TRANSFER_DST_OPTIMAL,
                A::empty(),
                A::TRANSFER_WRITE,
            );
            let region = [vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            let host = [vk::MemoryBarrier::default()
                .src_access_mask(A::TRANSFER_WRITE)
                .dst_access_mask(A::HOST_READ)];
            // SAFETY: `cb` is recording; handles outlive the submit.
            unsafe {
                d.cmd_clear_color_image(
                    cb,
                    img.image,
                    L::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue { float32: rgba },
                    &[RANGE],
                );
                barrier(
                    d,
                    cb,
                    L::TRANSFER_DST_OPTIMAL,
                    L::TRANSFER_SRC_OPTIMAL,
                    A::TRANSFER_WRITE,
                    A::TRANSFER_READ,
                );
                d.cmd_copy_image_to_buffer(
                    cb,
                    img.image,
                    L::TRANSFER_SRC_OPTIMAL,
                    hb.buffer,
                    &region,
                );
                d.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &host,
                    &[],
                    &[],
                );
            }
        })?;
        drop(hb);
        Ok(mem.read(n))
    }
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        // SAFETY: created in `create_image_rgba8` on this device; work has completed.
        unsafe {
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::gpu_or_skip;

    fn unorm(c: [f32; 4]) -> [u8; 4] {
        c.map(|v| (v * 255.0).round() as u8)
    }

    #[test]
    fn clear_and_read_matches_the_colour() {
        let Some(g) = gpu_or_skip() else { return };
        let cases = [
            (1, 1, [1.0, 0.0, 0.2, 1.0]),
            (64, 32, [0.4, 0.6, 0.8, 0.2]),
            (7, 5, [0.0, 0.0, 0.0, 0.0]),
        ];
        for (w, h, c) in cases {
            let px = g.clear_and_read(w, h, c).unwrap();
            assert_eq!(px.len(), (w * h * 4) as usize);
            let want = unorm(c);
            for (i, p) in px.chunks(4).enumerate() {
                assert_eq!(p, want, "pixel {i} of {w}x{h}");
            }
        }
    }

    #[test]
    fn empty_image_is_an_error() {
        let Some(g) = gpu_or_skip() else { return };
        assert!(g.clear_and_read(0, 4, [0.0; 4]).is_err());
    }
}
