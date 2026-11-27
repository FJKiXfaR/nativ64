//! Which registry views the draws of a submission can read (lazy residency, `textures.rs`).
//!
//! `scan` follows the state commands of each list as the replay will (`RootArgs`: the root
//! signature, its tables, the bound heaps) without recording anything, and at every draw reads
//! the descriptor slots the bound tables reach: a range of views (not samplers) of the signature gives
//! `count` slots from `handle + offset * 32`, an unbounded range or an "append" offset goes on
//! to the end of the heap holding the handle. A slot holding the registry tag names a view
//! index, which goes into the result. A signature that indexes the heaps directly marks every
//! slot of the bound resource heaps.
//!
//! A dispatch is read as a draw, from the compute root signature's tables (`RootArgs::compute`);
//! the unordered access views of its ranges are marked like the others (chunk 10c).
//!
//! shortcuts: no `ExecuteIndirect` (not replayed); with no
//! root signature set (nothing a title does) a table is read as 32-byte slots from its handle to
//! the end of its committed pages, at most `FALLBACK_SLOTS`; the slots read in one submission
//! stop at `SCAN_CAP`.

use super::log_once;
use super::pipeline::HeapReads;
use super::rootblock::RootArgs;
use crate::d3d12::command::Command;
use crate::d3d12::descriptor::{DescriptorHeapData, registry_index_at};
use crate::d3d12::rootobj::RootSignatureData;
use crate::d3d12::rootsig::{Binding, Range, RootSignature};
use crate::xobj::XObject;
use nativ64::api_ms_win_core_memory as table;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

/// Size of a view slot (`GetDescriptorHandleIncrementSize` of the view heaps).
const VIEW_SLOT: u64 = 32;
/// Descriptor range type of samplers.
const RANGE_SAMPLER: u32 = 3;
/// The root signature flags that let a shader index the resource and the sampler heaps.
const FLAG_RESOURCE_HEAP_DIRECT: u32 = 0x400;
/// The most slots read in one submission.
const SCAN_CAP: u64 = 1 << 23;
/// The slots read for a shader array of views, whose length is not known.
const ARRAY_SLOTS: u64 = 256;
/// The most slots of a table read when the list set no root signature.
const FALLBACK_SLOTS: u64 = 4096;

/// A bound descriptor heap of views: `start..end`.
#[derive(Clone, Copy)]
pub(super) struct Heap {
    start: u64,
    end: u64,
}

#[derive(Default)]
pub(super) struct Scanner {
    pub(super) wanted: HashSet<u32>,
    /// The (pipeline, signature, slot, handle) tables already read: pipelines differ in the
    /// slots their shaders read.
    seen: HashSet<(usize, usize, u32, u64)>,
    /// The heaps already marked whole.
    marked: HashSet<u64>,
    read: u64,
    capped: bool,
}

/// The registry indices that the draws of `lists` can read.
pub(super) fn scan(
    lists: &[Vec<Command>],
    reads: &mut HashMap<u64, HeapReads>,
) -> HashSet<u32> {
    let mut s = Scanner {
        wanted: HashSet::new(),
        seen: HashSet::new(),
        marked: HashSet::new(),
        read: 0,
        capped: false,
    };
    for list in lists {
        let mut args = RootArgs::default();
        let mut cargs = RootArgs::compute();
        let (mut signature, mut csignature) = (0usize, 0usize);
        let mut pipeline = 0usize;
        let mut heaps: Vec<Heap> = Vec::new();
        for c in list {
            match c {
                Command::Draw { .. } | Command::DrawIndexed { .. } => {
                    let r = reads_of(pipeline, reads);
                    s.draw(&args, pipeline, signature, &heaps, &r)
                }
                Command::Dispatch { .. } => {
                    let r = reads_of(pipeline, reads);
                    s.draw(&cargs, pipeline, csignature, &heaps, &r)
                }
                Command::SetPipeline { pipeline: p } => pipeline = *p,
                Command::ClearState { pipeline: p } => {
                    pipeline = *p;
                    args.apply(c);
                    cargs.apply(c);
                }
                Command::RootSignature {
                    compute,
                    signature: sig,
                } => {
                    if *compute {
                        csignature = *sig;
                    } else {
                        signature = *sig;
                    }
                    args.apply(c);
                    cargs.apply(c);
                }
                Command::Heaps(h) => {
                    heaps = heaps_of(h);
                    args.apply(c);
                    cargs.apply(c);
                }
                _ => {
                    args.apply(c);
                    cargs.apply(c);
                }
            }
        }
    }
    s.wanted
}

/// What the pipeline object `ptr` reads, by its serial.
fn reads_of(ptr: usize, cache: &mut HashMap<u64, HeapReads>) -> HeapReads {
    if ptr == 0 {
        return HeapReads::default();
    }
    // SAFETY: a pipeline object the list recorded.
    let Some(pd) = (unsafe { super::draw::pipeline_data(ptr) }) else {
        return HeapReads::default();
    };
    cache
        .entry(pd.serial)
        .or_insert_with(|| super::pipeline::heap_reads(pd))
        .clone()
}

/// The view heaps among `heaps` (objects the list recorded).
fn heaps_of(heaps: &[usize]) -> Vec<Heap> {
    let mut v = Vec::new();
    for &h in heaps {
        if h == 0 {
            continue;
        }
        // SAFETY: a heap object the list recorded; its data outlives the list.
        let d = unsafe { XObject::<DescriptorHeapData>::data(h as *mut c_void) };
        if d.desc.kind == 0 && d.start != 0 {
            v.push(Heap {
                start: d.start,
                end: d.start + u64::from(d.desc.count) * VIEW_SLOT,
            });
        }
    }
    v
}

impl Scanner {
    fn draw(
        &mut self,
        args: &RootArgs,
        pipeline: usize,
        signature: usize,
        heaps: &[Heap],
        reads: &HeapReads,
    ) {
        let model = (signature != 0).then(|| {
            // SAFETY: a root signature object the list recorded; its model is never mutated.
            let d = unsafe { XObject::<RootSignatureData>::data(signature as *mut c_void) };
            &d.model
        });
        let direct = reads.whole || model.is_some_and(|m| m.flags & FLAG_RESOURCE_HEAP_DIRECT != 0);
        if direct {
            for &h in heaps {
                if self.marked.insert(h.start) {
                    self.slots(h.start, (h.end - h.start) / VIEW_SLOT, Some(h));
                }
            }
        }
        for (&slot, &handle) in &args.tables {
            if handle == 0 || !self.seen.insert((pipeline, signature, slot, handle)) {
                continue;
            }
            let heap = heaps.iter().find(|h| (h.start..h.end).contains(&handle));
            for &(_, at, array) in reads.views.iter().filter(|v| v.0 == slot) {
                let first = handle + u64::from(at) * VIEW_SLOT;
                let n = if array { ARRAY_SLOTS } else { 1 };
                self.slots(first, n, heap.copied());
            }
            match model {
                Some(m) => self.table(m, slot, handle, heap),
                None => self.fallback(handle, heap),
            }
        }
    }

    /// The slots of the shader resource ranges of root parameter `slot`.
    pub(super) fn table(
        &mut self,
        m: &RootSignature,
        slot: u32,
        handle: u64,
        heap: Option<&Heap>,
    ) {
        let Some(Binding::Table(ranges)) = m.parameters.get(slot as usize).map(|p| &p.binding)
        else {
            return;
        };
        let mut next = 0u32;
        for r @ Range { count, offset, .. } in ranges {
            let at = if *offset == u32::MAX { next } else { *offset };
            next = at.saturating_add(*count);
            if r.kind == RANGE_SAMPLER {
                continue;
            }
            let first = handle.saturating_add(u64::from(at) * VIEW_SLOT);
            let n = if *count == u32::MAX || *offset == u32::MAX && *count == 0 {
                match heap {
                    Some(h) => h.end.saturating_sub(first) / VIEW_SLOT,
                    None => FALLBACK_SLOTS,
                }
            } else {
                u64::from(*count)
            };
            self.slots(first, n, heap.copied());
        }
    }

    fn fallback(&mut self, handle: u64, heap: Option<&Heap>) {
        let end = match heap {
            Some(h) => h.end,
            None => match table::committed_run(handle as usize) {
                Some((_, end)) => end as u64,
                None => {
                    log_once(format!("a root table at {handle:#x} is not readable"));
                    return;
                }
            },
        };
        let n = (end.saturating_sub(handle) / VIEW_SLOT).min(FALLBACK_SLOTS);
        self.slots(handle, n, heap.copied());
    }

    /// Reads `n` view slots from `first`; inside `heap` when it is known, else only where the
    /// pages are committed.
    fn slots(&mut self, first: u64, n: u64, heap: Option<Heap>) {
        let n = match heap {
            Some(h) if first < h.start || first >= h.end => {
                log_once(format!("a descriptor range at {first:#x} is outside its heap"));
                return;
            }
            Some(h) => n.min((h.end - first) / VIEW_SLOT),
            None => match table::committed_run(first as usize) {
                Some((_, end)) => n.min((end as u64).saturating_sub(first) / VIEW_SLOT),
                None => {
                    log_once(format!("a descriptor range at {first:#x} is not readable"));
                    return;
                }
            },
        };
        let n = n.min(SCAN_CAP.saturating_sub(self.read));
        if self.read >= SCAN_CAP && !self.capped {
            self.capped = true;
            log_once("the descriptor scan reached its cap: the rest is not made resident".into());
        }
        self.read += n;
        for i in 0..n {
            // SAFETY: inside the heap, or inside committed pages (checked above).
            if let Some(index) = unsafe { registry_index_at(first + i * VIEW_SLOT) } {
                self.wanted.insert(index);
            }
        }
    }
}

