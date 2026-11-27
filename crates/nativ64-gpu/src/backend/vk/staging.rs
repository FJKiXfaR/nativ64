//! A persistent, persistently mapped staging buffer for texture uploads.
//!
//! The uploads of a submission are bump-allocated in one host-visible buffer, which is reset
//! when the submission is done (the queue is idle between submissions). The buffer grows by
//! doubling when a submission needs more (the old one is kept until the submission is done,
//! the copies already recorded read it) and is freed after `IDLE_LIMIT` submissions that used
//! less than a quarter of it, so the host memory it pins follows the load.

use super::{vk_try, Gpu, Result, VkError};
use ash::vk;

/// Submissions using under a quarter of the buffer before it is freed.
const IDLE_LIMIT: u32 = 120;
const MIN_CAP: u64 = 1 << 20;
const ALIGN: u64 = 16;

struct Block {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    cap: u64,
}

/// See the module documentation.
#[derive(Default)]
pub(super) struct StagingRing {
    block: Option<Block>,
    /// Blocks replaced during the submission: freed with it.
    retired: Vec<Block>,
    used: u64,
    peak: u64,
    idle: u32,
    /// Buffers made since the start (tests).
    pub(super) made: u64,
}

// SAFETY: the pointer is the mapping of memory owned by the ring.
unsafe impl Send for StagingRing {}

fn free(g: &Gpu, b: Block) {
    // SAFETY: the submissions using the block are done.
    unsafe {
        g.device.destroy_buffer(b.buffer, None);
        g.device.free_memory(b.memory, None);
    }
}

fn make(g: &Gpu, cap: u64) -> Result<Block> {
    let d = &g.device;
    // SAFETY: handles are destroyed here on failure, else by `free`.
    unsafe {
        let buffer = vk_try(
            "vkCreateBuffer",
            d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(cap)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            ),
        )?;
        let req = d.get_buffer_memory_requirements(buffer);
        let flags = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
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
        let mapped = d
            .bind_buffer_memory(buffer, memory, 0)
            .and_then(|_| d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()));
        match mapped {
            Ok(p) => Ok(Block {
                buffer,
                memory,
                ptr: p.cast(),
                cap,
            }),
            Err(e) => {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
                Err(VkError::Vk("staging", e))
            }
        }
    }
}

impl StagingRing {
    /// `len` writable bytes in the mapped buffer and where they sit (`buffer`, offset, 16-byte
    /// aligned), valid until `end_submission`. A request that needs a bigger buffer makes one
    /// twice the size (or just enough), never beyond `max` unless `len` alone is larger.
    pub(super) fn alloc(
        &mut self,
        g: &Gpu,
        len: usize,
        max: u64,
    ) -> Result<(vk::Buffer, u64, &mut [u8])> {
        let len = len as u64;
        let at = self.used.next_multiple_of(ALIGN);
        let fits = self.block.as_ref().is_some_and(|b| at + len <= b.cap);
        if !fits {
            let cap = self
                .block
                .as_ref()
                .map_or(MIN_CAP, |b| b.cap * 2)
                .max(len.next_power_of_two())
                .min(max)
                .max(len)
                .max(1);
            let block = make(g, cap)?;
            self.made += 1;
            if let Some(old) = self.block.replace(block) {
                self.retired.push(old);
            }
            self.used = 0;
        }
        let at = self.used.next_multiple_of(ALIGN);
        self.used = at + len;
        self.peak = self.peak.max(self.used);
        let b = self.block.as_ref().expect("block");
        // SAFETY: `at + len <= cap`, inside the mapping; the range is handed out once until
        // the submission is done.
        let s = unsafe { std::slice::from_raw_parts_mut(b.ptr.add(at as usize), len as usize) };
        Ok((b.buffer, at, s))
    }

    /// Gives back the last `len` bytes of an allocation that was not used.
    pub(super) fn rollback(&mut self, len: usize) {
        self.used = self.used.saturating_sub(len as u64);
    }

    /// The submission is done: reuses the buffer, frees the retired ones and a buffer left
    /// mostly unused for a while.
    pub(super) fn end_submission(&mut self, g: &Gpu) {
        for b in self.retired.drain(..) {
            free(g, b);
        }
        if let Some(b) = &self.block {
            if self.peak * 4 < b.cap {
                self.idle += 1;
            } else {
                self.idle = 0;
            }
            if self.idle >= IDLE_LIMIT {
                if let Some(b) = self.block.take() {
                    free(g, b);
                }
                self.idle = 0;
            }
        }
        self.used = 0;
        self.peak = 0;
    }

    /// Frees everything (the queue is idle).
    pub(super) fn destroy(&mut self, g: &Gpu) {
        for b in self.retired.drain(..).chain(self.block.take()) {
            free(g, b);
        }
    }
}
