//! Static TLS: the image's TLS directory, per-thread blocks and TLS callbacks.

use crate::pe::{rd_u32, rd_u64};
use crate::LoadError;
use std::alloc::Layout;

const DLL_PROCESS_ATTACH: u32 = 1;
const SLOTS: usize = 64;
/// How many images (the exe and its DLLs) can have static TLS.
pub const MAX_TLS_IMAGES: usize = SLOTS;

/// What the loader keeps of the TLS directory (addresses are mapped VAs).
#[derive(Clone, Debug)]
pub struct TlsInfo {
    pub raw_start: u64,
    pub raw_end: u64,
    pub zero_fill: u32,
    pub callbacks: Vec<u64>,
}

impl TlsInfo {
    /// Parses the directory at `rva` inside the mapped (relocated) image `mem` loaded at
    /// `base`, validates every range, and returns it with the RVA of `AddressOfIndex`.
    pub fn parse(
        mem: &[u8],
        base: u64,
        rva: u32,
        size: u32,
    ) -> Result<(TlsInfo, usize), LoadError> {
        let bad = LoadError::BadTls;
        if size < 40 {
            return Err(bad("directory too small"));
        }
        let d = rva as usize;
        if d.checked_add(40).is_none_or(|e| e > mem.len()) {
            return Err(bad("directory out of bounds"));
        }
        let raw_start = rd_u64(mem, d, "TLS directory")?;
        let raw_end = rd_u64(mem, d + 8, "TLS directory")?;
        let index_va = rd_u64(mem, d + 16, "TLS directory")?;
        let callbacks_va = rd_u64(mem, d + 24, "TLS directory")?;
        let zero_fill = rd_u32(mem, d + 32, "TLS directory")?;
        let to_rva = |va: u64, len: usize| -> Option<usize> {
            let r = va.checked_sub(base)? as usize;
            (r.checked_add(len)? <= mem.len()).then_some(r)
        };
        if raw_end < raw_start {
            return Err(bad("raw data end before start"));
        }
        let raw_len = (raw_end - raw_start) as usize;
        if to_rva(raw_start, raw_len).is_none() {
            return Err(bad("raw data out of bounds"));
        }
        let Some(index_rva) = to_rva(index_va, 4) else {
            return Err(bad("AddressOfIndex out of bounds"));
        };
        if raw_len.saturating_add(zero_fill as usize) > 1 << 28 {
            return Err(bad("TLS block too large"));
        }
        let mut callbacks = Vec::new();
        if callbacks_va != 0 {
            let Some(mut at) = to_rva(callbacks_va, 8) else {
                return Err(bad("callback array out of bounds"));
            };
            loop {
                let cb = rd_u64(mem, at, "TLS callbacks")?;
                if cb == 0 {
                    break;
                }
                if to_rva(cb, 1).is_none() {
                    return Err(bad("callback out of bounds"));
                }
                callbacks.push(cb);
                at += 8;
                if at + 8 > mem.len() {
                    return Err(bad("callback array not terminated"));
                }
            }
        }
        Ok((
            TlsInfo {
                raw_start,
                raw_end,
                zero_fill,
                callbacks,
            },
            index_rva,
        ))
    }
}

/// One thread's TLS block and slot array. Keep alive as long as the thread runs guest code.
pub struct ThreadTls {
    blocks: Vec<(*mut u8, Layout)>,
    slots: Box<[usize; SLOTS]>,
}

impl ThreadTls {
    /// Allocates one block per `(index, info)` (raw data copied, the rest zero) and fills its
    /// slot. An index must be below the slot count (see [`MAX_TLS_IMAGES`]).
    ///
    /// # Safety
    /// Every `info` must describe a mapped, readable range (as validated by [`TlsInfo::parse`]).
    pub unsafe fn new(images: &[(usize, TlsInfo)]) -> ThreadTls {
        let mut slots = Box::new([0usize; SLOTS]);
        let mut blocks = Vec::new();
        for (index, info) in images {
            let raw_len = (info.raw_end - info.raw_start) as usize;
            let total = (raw_len + info.zero_fill as usize).max(1);
            let layout = Layout::from_size_align(total, 64).expect("TLS layout");
            let block = std::alloc::alloc_zeroed(layout);
            assert!(!block.is_null(), "out of memory for the TLS block");
            std::ptr::copy_nonoverlapping(info.raw_start as *const u8, block, raw_len);
            slots[*index] = block as usize;
            blocks.push((block, layout));
        }
        ThreadTls { blocks, slots }
    }

    /// The value for `TEB.ThreadLocalStoragePointer`.
    pub fn array(&mut self) -> *mut usize {
        self.slots.as_mut_ptr()
    }
}

impl Drop for ThreadTls {
    fn drop(&mut self) {
        for &(block, layout) in &self.blocks {
            unsafe { std::alloc::dealloc(block, layout) };
        }
    }
}

/// Calls every TLS callback with `DLL_PROCESS_ATTACH`.
///
/// # Safety
/// The callbacks are guest code: the TEB must be installed on this thread.
pub unsafe fn run_callbacks(info: &TlsInfo, image_base: u64) {
    for &cb in &info.callbacks {
        let f: extern "win64" fn(u64, u32, usize) = std::mem::transmute(cb as usize);
        f(image_base, DLL_PROCESS_ATTACH, 0);
    }
}
