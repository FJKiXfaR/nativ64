//! GPU memory (chunk 10a, `docs/shaders.md`, Chunk 10 design): title addresses are host
//! addresses, so the backend imports the host pages a draw reads and hands the shader (and the
//! vertex and index bindings) device addresses translated **on the CPU**.
//!
//! `GpuMemory` keeps the imports by host range. A miss imports the run of committed pages the
//! memory table reports around the address (`nativ64::api_ms_win_core_memory::committed_run`),
//! or the memory of a descriptor heap that owns its own (`descriptor::heap_ranges`); anything
//! else (an address outside both, a page that is not committed) does not translate, and the
//! caller skips the draw. The ranges the table or a heap records as changed
//! (`take_gpu_changes`) drop the imports overlapping them at the start of each submission:
//! the queue is idle between submissions, so a dropped import is never in use.
//!
//! shortcuts: a run is imported whole (a huge committed run is one buffer: a driver limit on
//! the allocation size surfaces as an import error, logged, and the draw is skipped); the run
//! is shrunk inwards to the import alignment, so a run smaller than it cannot be imported;
//! a pointer stored in memory is not translated (the stage is skipped instead, see
//! `Translated::memory_addresses`).

use super::{log_once, Gpu, HostBuffer};
use ash::vk;
use nativ64::api_ms_win_core_memory as table;
use std::collections::BTreeMap;

/// Device addresses must stay below this (a built V# keeps 48 bits of base).
const ADDRESS_LIMIT: u64 = 1 << 48;

/// Where a title address lives on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapped {
    pub buffer: vk::Buffer,
    /// Offset of the address in `buffer`.
    pub offset: u64,
    /// The device address of the same byte.
    pub address: vk::DeviceAddress,
}

struct Import<'g> {
    end: usize,
    buffer: HostBuffer<'g>,
}

pub struct GpuMemory<'g> {
    gpu: &'g Gpu,
    imports: BTreeMap<usize, Import<'g>>,
}

impl<'g> GpuMemory<'g> {
    pub fn new(gpu: &'g Gpu) -> Self {
        table::watch_gpu_changes();
        GpuMemory {
            gpu,
            imports: BTreeMap::new(),
        }
    }

    /// How many ranges are imported.
    #[cfg(test)]
    pub fn imports(&self) -> usize {
        self.imports.len()
    }

    /// Drops the imports overlapping a range the memory table or a heap recorded as changed.
    /// Called with the GPU idle. True when anything changed (device addresses may differ now).
    pub fn sync(&mut self) -> bool {
        let changes = table::take_gpu_changes();
        let any = !changes.is_empty();
        if any {
            self.forget(&changes);
        }
        any
    }

    pub fn forget(&mut self, changes: &[(usize, usize)]) {
        self.imports.retain(|&start, i| {
            !changes
                .iter()
                .any(|&(a, n)| a < i.end && start < a.saturating_add(n))
        });
    }

    /// The device side of `len` bytes (at least 1) at title address `va`, importing the memory
    /// on first use; None when the address is not memory we can import, or the range runs out
    /// of what is imported.
    pub fn translate(&mut self, va: u64, len: u64) -> Option<Mapped> {
        let at = usize::try_from(va).ok()?;
        let end = at.checked_add(usize::try_from(len.max(1)).ok()?)?;
        if let Some((&start, i)) = self.imports.range(..=at).next_back() {
            if at < i.end {
                return (end <= i.end).then(|| Self::mapped(start, i, at));
            }
        }
        let (start, run_end) = self.run_around(at)?;
        let align = self.gpu.import_alignment().max(1);
        let (start, run_end) = (start.next_multiple_of(align), run_end / align * align);
        if !(start <= at && end <= run_end) {
            log_once(format!(
                "GPU memory: {va:#x} (+{len:#x}) does not fit an import of {start:#x}..{run_end:#x} \
                 (alignment {align:#x})"
            ));
            return None;
        }
        let buffer = match self.gpu.import_host(start as *mut u8, run_end - start) {
            Ok(b) => b,
            Err(e) => {
                log_once(format!(
                    "GPU memory: cannot import {start:#x}..{run_end:#x}: {e}"
                ));
                return None;
            }
        };
        if (buffer.address + buffer.len as u64) >= ADDRESS_LIMIT {
            log_once(format!(
                "GPU memory: the device address {:#x} of {start:#x}.. is above 2^48",
                buffer.address
            ));
            return None;
        }
        let import = Import {
            end: run_end,
            buffer,
        };
        let m = Self::mapped(start, &import, at);
        self.imports.insert(start, import);
        Some(m)
    }

    /// The host range around `at` an import may cover: the committed run of the memory table,
    /// else the memory of a descriptor heap.
    fn run_around(&self, at: usize) -> Option<(usize, usize)> {
        table::committed_run(at).or_else(|| {
            crate::d3d12::descriptor::heap_ranges()
                .into_iter()
                .find(|&(a, n)| a <= at && at < a + n)
                .map(|(a, n)| (a, a + n))
        })
    }

    fn mapped(start: usize, i: &Import<'_>, at: usize) -> Mapped {
        let offset = (at - start) as u64;
        Mapped {
            buffer: i.buffer.buffer,
            offset,
            address: i.buffer.address + offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::gpu_or_skip;
    use super::*;

    const RW: u32 = 0x04;
    const COMMIT: u32 = table::MEM_COMMIT;
    const RESERVE: u32 = table::MEM_RESERVE;

    #[test]
    fn imports_on_demand_reuses_and_forgets_changed_ranges() {
        let Some(g) = gpu_or_skip() else { return };
        let mut m = GpuMemory::new(&g);
        let a = table::alloc(0, 0x40000, RESERVE | COMMIT, RW, None).unwrap();
        let first = m.translate(a as u64 + 0x100, 16).unwrap();
        assert_eq!(m.imports(), 1);
        // the same run again, further in: the same buffer
        let second = m.translate(a as u64 + 0x2100, 4).unwrap();
        assert_eq!(m.imports(), 1);
        assert_eq!(first.buffer, second.buffer);
        assert_eq!(second.address - first.address, 0x2000);
        assert_eq!(first.offset, 0x100);
        // a range running past the import does not translate
        assert!(m.translate(a as u64 + 0x3fff0, 0x20).is_none());
        // a decommitted page is recorded as changed: the import is dropped, and the page no
        // longer translates, while its neighbours import again
        table::free(a + 0x1000, 0x1000, table::MEM_DECOMMIT).unwrap();
        m.sync();
        assert_eq!(m.imports(), 0);
        assert!(m.translate(a as u64 + 0x1000, 4).is_none());
        assert!(m.translate(a as u64 + 0x2000, 4).is_some());
        assert_eq!(m.imports(), 1);
        table::release(a).unwrap();
        m.sync();
        assert_eq!(m.imports(), 0);
    }

    #[test]
    fn memory_outside_the_table_does_not_translate() {
        let Some(g) = gpu_or_skip() else { return };
        let mut m = GpuMemory::new(&g);
        let v = vec![0u8; 8192];
        assert!(m.translate(v.as_ptr() as u64, 4).is_none());
        assert!(m.translate(0, 4).is_none());
        assert_eq!(m.imports(), 0);
    }

    #[test]
    fn descriptor_heap_memory_translates() {
        use crate::d3d12::descriptor::{create_descriptor_heap, heap_ranges};
        let Some(g) = gpu_or_skip() else { return };
        let mut m = GpuMemory::new(&g);
        // kind 0 (views), 4 descriptors; the zero words after the description are no placement
        let desc = [0u32, 4, 0, 0, 0, 0, 0, 0];
        let mut out = std::ptr::null_mut();
        // SAFETY: a description with room after it, and a valid out pointer.
        let hr = unsafe {
            create_descriptor_heap(
                std::ptr::null_mut(),
                desc.as_ptr().cast(),
                &crate::d3d12::descriptor::DESCRIPTOR_HEAP_IIDS[0],
                &mut out,
            )
        };
        assert_eq!(hr, 0);
        let (a, n) = *heap_ranges().last().unwrap();
        assert!(m.translate(a as u64 + 8, 8).is_some());
        assert!(m.translate((a + n) as u64, 4).is_none());
    }
}
