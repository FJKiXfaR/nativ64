//! Zero-copy import of host memory (`VK_EXT_external_memory_host`): a `VkBuffer` bound to
//! memory that is the caller's own pages, with a queryable device address.

use super::{vk_try, Gpu, Result, VkError};
use ash::vk;
use std::alloc::Layout;
use std::ffi::c_void;

/// A buffer over host memory the caller keeps alive and mapped for as long as it lives.
pub struct HostBuffer<'g> {
    gpu: &'g Gpu,
    pub buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    pub len: usize,
    pub address: vk::DeviceAddress,
}

impl Gpu {
    /// Import `len` bytes at `ptr`. Both must be multiples of `import_alignment()`, else
    /// `VkError::Misaligned`. The memory must stay valid until the buffer is dropped.
    pub fn import_host(&self, ptr: *mut u8, len: usize) -> Result<HostBuffer<'_>> {
        let align = self.import_align.max(1);
        if len == 0 {
            return Err(VkError::Invalid("zero length"));
        }
        if !(ptr as usize).is_multiple_of(align) {
            return Err(VkError::Misaligned {
                value: ptr as usize,
                align,
            });
        }
        if !len.is_multiple_of(align) {
            return Err(VkError::Misaligned { value: len, align });
        }
        let handle = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        let d = &self.device;
        // SAFETY: the pointer is aligned and the caller guarantees it is valid memory; each
        // object is destroyed on the error paths below.
        unsafe {
            let mut hp = vk::MemoryHostPointerPropertiesEXT::default();
            let r = (self.ext_host.fp().get_memory_host_pointer_properties_ext)(
                self.ext_host.device(),
                handle,
                ptr as *const c_void,
                &mut hp,
            );
            if r != vk::Result::SUCCESS {
                return Err(VkError::Vk("vkGetMemoryHostPointerPropertiesEXT", r));
            }
            // The buffer must name the handle type of the memory it will be bound to.
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle);
            let buffer = vk_try(
                "vkCreateBuffer",
                d.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(len as u64)
                        .usage(
                            vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                                | vk::BufferUsageFlags::TRANSFER_SRC
                                | vk::BufferUsageFlags::TRANSFER_DST,
                        )
                        .push_next(&mut ext),
                    None,
                ),
            )?;
            let req = d.get_buffer_memory_requirements(buffer);
            let Some(ty) = self.memory_type(
                hp.memory_type_bits & req.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            ) else {
                d.destroy_buffer(buffer, None);
                return Err(VkError::NoMemoryType);
            };
            let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
                .handle_type(handle)
                .host_pointer(ptr as *mut c_void);
            let mut flags = vk::MemoryAllocateFlagsInfo::default()
                .flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(len as u64)
                    .memory_type_index(ty)
                    .push_next(&mut import)
                    .push_next(&mut flags),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(VkError::Vk("vkAllocateMemory", e));
                }
            };
            if let Err(e) = d.bind_buffer_memory(buffer, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_buffer(buffer, None);
                return Err(VkError::Vk("vkBindBufferMemory", e));
            }
            let address =
                d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer));
            Ok(HostBuffer {
                gpu: self,
                buffer,
                memory,
                len,
                address,
            })
        }
    }
}

impl Drop for HostBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: created by `import_host` on this device; callers wait for their work.
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Zeroed host memory aligned (and sized) for import, freed on drop.
pub struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    /// At least `len` bytes, rounded up to a multiple of `align` (a power of two), aligned to
    /// `align` and at least a page.
    pub fn new(len: usize, align: usize) -> AlignedBuf {
        let align = align.max(4096);
        let layout = Layout::from_size_align(len.max(1).next_multiple_of(align), align).unwrap();
        // SAFETY: non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory");
        AlignedBuf { ptr, layout }
    }
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.layout.size()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    /// Read the bytes; volatile because the GPU wrote them behind the compiler's back.
    pub fn read(&self, n: usize) -> Vec<u8> {
        assert!(n <= self.len());
        // SAFETY: within the allocation.
        (0..n)
            .map(|i| unsafe { self.ptr.add(i).read_volatile() })
            .collect()
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::gpu_or_skip;
    use super::*;

    #[test]
    fn rejects_misaligned_pointer_and_length() {
        let Some(g) = gpu_or_skip() else { return };
        let align = g.import_alignment();
        let buf = AlignedBuf::new(2 * align, align);
        let p = buf.as_ptr();
        // Pointer off by one byte (or by half the alignment when it is 1: skip then).
        if align > 1 {
            let e = g.import_host(unsafe { p.add(1) }, align).err().unwrap();
            assert!(matches!(e, VkError::Misaligned { .. }), "{e}");
            let e = g.import_host(p, align + 1).err().unwrap();
            assert!(matches!(e, VkError::Misaligned { .. }), "{e}");
        }
        assert!(matches!(
            g.import_host(p, 0).err().unwrap(),
            VkError::Invalid(_)
        ));
    }

    #[test]
    fn fill_buffer_is_seen_in_the_mapping() {
        let Some(g) = gpu_or_skip() else { return };
        let align = g.import_alignment();
        // A real mmap'ed page range, as title memory is.
        let len = 4096usize.max(align);
        // SAFETY: anonymous private mapping, unmapped below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        } as *mut u8;
        assert_ne!(p as isize, -1);
        let base = (p as usize).next_multiple_of(align) as *mut u8;
        {
            let hb = g.import_host(base, len).unwrap();
            assert_ne!(hb.address, 0);
            g.submit_once(|d, cb| unsafe {
                d.cmd_fill_buffer(cb, hb.buffer, 0, hb.len as u64, 0xdead_beef);
            })
            .unwrap();
            for i in 0..len / 4 {
                // SAFETY: inside the mapping.
                let v = unsafe { (base as *const u32).add(i).read_volatile() };
                assert_eq!(v, 0xdead_beef, "word {i}");
            }
        }
        // SAFETY: matches the mmap above; the buffer is gone.
        unsafe { libc::munmap(p as *mut c_void, len * 2) };
    }
}
