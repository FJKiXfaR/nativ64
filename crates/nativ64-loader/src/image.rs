//! Mapping, relocation and import binding.

use crate::pe::*;
use crate::stubs::{errno, StubPage};
use crate::tls::{self, ThreadTls, TlsInfo};
use crate::LoadError;

#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Try this address instead of the header's ImageBase (relocations still apply against
    /// the header's ImageBase). Mostly useful to test relocation.
    pub base_hint: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ImportEntry {
    pub module: String,
    /// Function name, or `#ordinal`.
    pub name: String,
    pub resolved: bool,
}

impl ImportEntry {
    pub fn display(&self) -> String {
        format!("{}!{}", self.module, self.name)
    }
}

/// A mapped, relocated and import-bound image. Unmapped on drop.
pub struct Image {
    base: *mut u8,
    size: usize,
    pub preferred_base: u64,
    pub entry_rva: u32,
    pub section_count: usize,
    pub imports: Vec<ImportEntry>,
    tls: Option<TlsInfo>,
    _stubs: Option<StubPage>,
}

impl Image {
    pub fn load(data: &[u8], opts: &LoadOptions) -> Result<Image, LoadError> {
        let pe = Pe::parse(data)?;
        let size = (pe.size_of_image as usize).div_ceil(4096) * 4096;
        let base = map_anywhere_or_at(opts.base_hint.unwrap_or(pe.image_base), size)?;
        let mut img = Image {
            base,
            size,
            preferred_base: pe.image_base,
            entry_rva: pe.entry_rva,
            section_count: pe.sections.len(),
            imports: Vec::new(),
            tls: None,
            _stubs: None,
        };
        img.copy_sections(&pe);
        img.relocate(&pe)?;
        img.bind_imports(&pe)?;
        img.setup_tls(&pe)?;
        img.protect(&pe)?;
        Ok(img)
    }

    pub fn base(&self) -> u64 {
        self.base as u64
    }

    /// Size in bytes of the mapping.
    pub fn size(&self) -> usize {
        self.size
    }

    pub fn entry_point(&self) -> u64 {
        self.base() + self.entry_rva as u64
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &ImportEntry> {
        self.imports.iter().filter(|i| !i.resolved)
    }

    /// Bytes of the mapped image at `rva` (valid until the image is dropped).
    pub fn bytes_at(&self, rva: u32, len: usize) -> Option<&[u8]> {
        let end = (rva as usize).checked_add(len)?;
        if end > self.size {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.base.add(rva as usize), len) })
    }

    fn mem<'a>(&self) -> &'a mut [u8] {
        // Only used while loading, before the image is shared and while it is writable.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.size) }
    }

    fn copy_sections(&mut self, pe: &Pe) {
        let hdr = pe.size_of_headers as usize;
        let mem = self.mem();
        mem[..hdr].copy_from_slice(&pe.data[..hdr]);
        // The mapping is anonymous, hence already zero: only the raw bytes need copying.
        for s in &pe.sections {
            let n = s.raw_size.min(s.mem_size()) as usize;
            let src = &pe.data[s.raw_offset as usize..s.raw_offset as usize + n];
            let dst = s.virtual_address as usize;
            mem[dst..dst + n].copy_from_slice(src);
        }
    }

    fn relocate(&mut self, pe: &Pe) -> Result<(), LoadError> {
        let delta = self.base().wrapping_sub(pe.image_base);
        if delta == 0 {
            return Ok(());
        }
        let dir = pe.dirs[DIR_BASERELOC];
        if dir.rva == 0 || dir.size == 0 {
            return Err(LoadError::NoRelocations);
        }
        let mem = self.mem();
        let end = (dir.rva as usize)
            .checked_add(dir.size as usize)
            .filter(|&e| e <= mem.len())
            .ok_or(LoadError::RvaOutOfBounds(dir.rva))?;
        let mut pos = dir.rva as usize;
        while pos + 8 <= end {
            let page = rd_u32(mem, pos, "relocation block")? as usize;
            let block = rd_u32(mem, pos + 4, "relocation block")? as usize;
            if block < 8 || pos + block > end {
                return Err(LoadError::BadHeader("bad relocation block size"));
            }
            for e in (pos + 8..pos + block).step_by(2) {
                let v = rd_u16(mem, e, "relocation entry")?;
                let (ty, off) = ((v >> 12) as u8, (v & 0xfff) as usize);
                match ty {
                    0 => {}
                    10 => {
                        let at = page + off;
                        if at + 8 > mem.len() {
                            return Err(LoadError::RvaOutOfBounds(at as u32));
                        }
                        let old = rd_u64(mem, at, "relocation target")?;
                        mem[at..at + 8].copy_from_slice(&old.wrapping_add(delta).to_le_bytes());
                    }
                    t => return Err(LoadError::UnsupportedReloc(t)),
                }
            }
            pos += block;
        }
        Ok(())
    }

    fn cstr(&self, rva: u32) -> Result<String, LoadError> {
        let mem = self.mem();
        let start = rva as usize;
        let tail = mem.get(start..).ok_or(LoadError::RvaOutOfBounds(rva))?;
        let n = tail
            .iter()
            .position(|&b| b == 0)
            .ok_or(LoadError::RvaOutOfBounds(rva))?;
        Ok(String::from_utf8_lossy(&tail[..n]).into_owned())
    }

    fn bind_imports(&mut self, pe: &Pe) -> Result<(), LoadError> {
        let dir = pe.dirs[DIR_IMPORT];
        // (iat slot rva, import index) of the imports that need a trap stub
        let mut pending: Vec<(usize, usize)> = Vec::new();
        if dir.rva != 0 {
            let mut desc = dir.rva as usize;
            loop {
                let mem = self.mem();
                if desc + 20 > mem.len() {
                    return Err(LoadError::RvaOutOfBounds(desc as u32));
                }
                let oft = rd_u32(mem, desc, "import descriptor")?;
                let name_rva = rd_u32(mem, desc + 12, "import descriptor")?;
                let ft = rd_u32(mem, desc + 16, "import descriptor")?;
                if oft == 0 && name_rva == 0 && ft == 0 {
                    break;
                }
                let module = self.cstr(name_rva)?;
                let lookup_table = if oft != 0 { oft } else { ft } as usize;
                for i in 0.. {
                    let mem = self.mem();
                    let at = lookup_table + i * 8;
                    let slot = ft as usize + i * 8;
                    if at + 8 > mem.len() || slot + 8 > mem.len() {
                        return Err(LoadError::RvaOutOfBounds(at as u32));
                    }
                    let thunk = rd_u64(mem, at, "import thunk")?;
                    if thunk == 0 {
                        break;
                    }
                    let name = if thunk >> 63 != 0 {
                        format!("#{}", thunk & 0xffff)
                    } else {
                        self.cstr((thunk as u32).wrapping_add(2))?
                    };
                    let found = if thunk >> 63 != 0 {
                        None
                    } else {
                        nativ64::lookup(&module, &name)
                    };
                    let idx = self.imports.len();
                    self.imports.push(ImportEntry {
                        module: module.clone(),
                        name,
                        resolved: found.is_some(),
                    });
                    match found {
                        Some(e) => {
                            let a = e.addr as u64;
                            self.mem()[slot..slot + 8].copy_from_slice(&a.to_le_bytes());
                        }
                        None => pending.push((slot, idx)),
                    }
                }
                desc += 20;
            }
        }
        if !pending.is_empty() {
            let names: Vec<String> = pending
                .iter()
                .map(|&(_, i)| self.imports[i].display())
                .collect();
            let stubs = StubPage::new(&names)?;
            for (k, &(slot, _)) in pending.iter().enumerate() {
                let a = stubs.addr(k);
                self.mem()[slot..slot + 8].copy_from_slice(&a.to_le_bytes());
            }
            self._stubs = Some(stubs);
        }
        Ok(())
    }

    /// Validates the TLS directory and writes TLS index 0 (the image is still writable).
    fn setup_tls(&mut self, pe: &Pe) -> Result<(), LoadError> {
        let dir = pe.dirs[DIR_TLS];
        if dir.rva == 0 || dir.size == 0 {
            return Ok(());
        }
        let (info, index_rva) = TlsInfo::parse(self.mem(), self.base(), dir.rva, dir.size)?;
        self.mem()[index_rva..index_rva + 4].copy_from_slice(&0u32.to_le_bytes());
        self.tls = Some(info);
        Ok(())
    }

    /// Runs the entry point as an EXE `extern "win64" fn() -> u32` on a new thread with a
    /// 64 MiB stack, a TEB/PEB and static TLS, after the TLS callbacks; returns its result.
    /// A guest crash is the caller's business (see [`crate::fault`]).
    pub fn run_main(&self) -> Result<u32, LoadError> {
        if self.entry_rva == 0 {
            return Err(LoadError::NoEntryPoint);
        }
        let (base, entry) = (self.base(), self.entry_point());
        let tls = self.tls.clone();
        let t = std::thread::Builder::new()
            .name("nativ64-main".into())
            .stack_size(64 << 20)
            .spawn(move || -> Result<u32, LoadError> {
                crate::fault::setup_thread();
                let (lo, hi) = current_stack()?;
                crate::teb::set_image_base(base)?;
                let teb = crate::teb::install_for_current_thread(hi, lo)?;
                let mut block = tls.as_ref().map(|i| unsafe { ThreadTls::new(i) });
                if let Some(b) = block.as_mut() {
                    teb.set_tls_pointer(b.array());
                }
                if let Some(i) = &tls {
                    unsafe { tls::run_callbacks(i, base) };
                }
                // SAFETY: the entry point of a mapped image; an EXE entry takes no argument.
                let f: extern "win64" fn() -> u32 = unsafe { std::mem::transmute(entry as usize) };
                Ok(f())
            })
            .map_err(|e| LoadError::Os("spawn", e.raw_os_error().unwrap_or(0)))?;
        t.join().unwrap_or(Err(LoadError::Os("join", 0)))
    }

    fn protect(&mut self, pe: &Pe) -> Result<(), LoadError> {
        let ro = |p: *mut u8, len: usize, prot: i32| -> Result<(), LoadError> {
            if unsafe { libc::mprotect(p.cast(), len, prot) } != 0 {
                return Err(LoadError::Os("mprotect", errno()));
            }
            Ok(())
        };
        ro(self.base, self.size, libc::PROT_READ)?;
        for s in &pe.sections {
            let mut prot = 0;
            if s.characteristics & SCN_MEM_READ != 0 {
                prot |= libc::PROT_READ;
            }
            if s.characteristics & SCN_MEM_WRITE != 0 {
                prot |= libc::PROT_WRITE;
            }
            if s.characteristics & SCN_MEM_EXECUTE != 0 {
                prot |= libc::PROT_EXEC;
            }
            let len = (s.mem_size() as usize).div_ceil(4096) * 4096;
            ro(
                unsafe { self.base.add(s.virtual_address as usize) },
                len,
                prot,
            )?;
        }
        Ok(())
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base.cast(), self.size) };
    }
}

fn map_anywhere_or_at(want: u64, size: usize) -> Result<*mut u8, LoadError> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    let p = unsafe {
        libc::mmap(
            want as *mut libc::c_void,
            size,
            prot,
            flags | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if p != libc::MAP_FAILED {
        return Ok(p.cast());
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        return Err(LoadError::Os("mmap(image)", errno()));
    }
    Ok(p.cast())
}

/// (low, high) bounds of the calling thread's stack.
fn current_stack() -> Result<(usize, usize), LoadError> {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let r = libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
        if r != 0 {
            return Err(LoadError::Os("pthread_getattr_np", r));
        }
        let mut addr = std::ptr::null_mut();
        let mut size = 0usize;
        libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        Ok((addr as usize, addr as usize + size))
    }
}
