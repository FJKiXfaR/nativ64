//! Mapping, relocation and import binding.

use crate::exports::{self, Exports, RawTarget};
use crate::pe::*;
use crate::stubs::{errno, StubPage, TracePage};
use crate::tls::{self, ThreadTls, TlsInfo, MAX_TLS_IMAGES};
use crate::LoadError;
use nativ64::process::ExportRef;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Try this address instead of the header's ImageBase (relocations still apply against
    /// the header's ImageBase). Mostly useful to test relocation.
    pub base_hint: Option<u64>,
    /// Route every resolved import through a trampoline that logs `nativ64: call module!name`.
    pub trace_imports: bool,
    /// Directories searched for the DLLs the image imports (the package root first, then the
    /// extra directories). An import module that exists there as a file (name compared
    /// ignoring case) is mapped and bound; the others go to nativ64's own modules.
    pub search_dirs: Vec<PathBuf>,
    /// Modules nativ64 provides even when the package ships a file of that name.
    pub native_modules: Vec<String>,
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
    /// Module name (lowercase file name) of a DLL; empty for the main image.
    name: String,
    path: String,
    /// Package DLLs mapped for this image, dependencies first (main image only).
    dlls: Vec<Image>,
    exports: Arc<Exports>,
    /// TLS index (slot in `ThreadLocalStoragePointer`) and directory.
    tls: Option<(usize, TlsInfo)>,
    _stubs: Option<StubPage>,
    _trace: Option<TracePage>,
}

impl Image {
    pub fn load(data: &[u8], opts: &LoadOptions) -> Result<Image, LoadError> {
        let mut ctx = Ctx::new(opts);
        let mut img = Image::load_one(data, &mut ctx, opts.base_hint, None)?;
        img.dlls = std::mem::take(&mut ctx.dlls);
        Ok(img)
    }

    /// Maps one image (the main one, or a DLL given as `(name, path)`).
    fn load_one(
        data: &[u8],
        ctx: &mut Ctx,
        base_hint: Option<u64>,
        dll: Option<(String, PathBuf)>,
    ) -> Result<Image, LoadError> {
        let pe = Pe::parse(data)?;
        let size = (pe.size_of_image as usize).div_ceil(4096) * 4096;
        let base = map_anywhere_or_at(base_hint.unwrap_or(pe.image_base), size)?;
        let is_dll = dll.is_some();
        let (name, path) = dll.unzip();
        let mut img = Image {
            base,
            size,
            preferred_base: pe.image_base,
            entry_rva: pe.entry_rva,
            section_count: pe.sections.len(),
            imports: Vec::new(),
            name: name.unwrap_or_default(),
            path: path
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dlls: Vec::new(),
            exports: Arc::new(Exports::default()),
            tls: None,
            _stubs: None,
            _trace: None,
        };
        img.copy_sections(&pe);
        img.relocate(&pe)?;
        img.bind_imports(&pe, ctx)?;
        if is_dll {
            img.resolve_exports(&pe, ctx)?;
        }
        img.setup_tls(&pe, ctx, is_dll)?;
        img.protect(&pe)?;
        Ok(img)
    }

    /// Module name of a package DLL (lowercase file name); empty for the main image.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host path of a package DLL; empty for the main image.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The package DLLs mapped for this image, in dependency order (dependencies first).
    pub fn dlls(&self) -> &[Image] {
        &self.dlls
    }

    /// Address of an export of this (DLL) image, forwarders resolved.
    pub fn export(&self, r: ExportRef<'_>) -> Option<u64> {
        self.exports.get(r)
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

    fn bind_imports(&mut self, pe: &Pe, ctx: &mut Ctx) -> Result<(), LoadError> {
        let trace = ctx.opts.trace_imports;
        let dir = pe.dirs[DIR_IMPORT];
        // (iat slot rva, import index) of the imports that need a trap stub
        let mut pending: Vec<(usize, usize)> = Vec::new();
        // (iat slot rva, name, target) of the resolved imports to trace
        let mut traced: Vec<(usize, String, u64)> = Vec::new();
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
                // A package file of that name, mapped now (with its own dependencies first).
                let dll = ctx.ensure(&module)?;
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
                    let by_ordinal = thunk >> 63 != 0;
                    let name = if by_ordinal {
                        format!("#{}", thunk & 0xffff)
                    } else {
                        self.cstr((thunk as u32).wrapping_add(2))?
                    };
                    let from_dll = dll.and_then(|d| {
                        let r = if by_ordinal {
                            ExportRef::Ordinal((thunk & 0xffff) as u16)
                        } else {
                            ExportRef::Name(&name)
                        };
                        ctx.dlls[d].exports.get(r)
                    });
                    let found = from_dll.or_else(|| {
                        if by_ordinal {
                            nativ64::lookup_ordinal(&module, (thunk & 0xffff) as u16)
                        } else {
                            nativ64::lookup(&module, &name)
                                .or_else(|| nativ64_gpu::lookup(&module, &name))
                        }
                        .map(|e| e.addr as u64)
                    });
                    let idx = self.imports.len();
                    let shown = format!("{module}!{name}");
                    self.imports.push(ImportEntry {
                        module: module.clone(),
                        name,
                        resolved: found.is_some(),
                    });
                    match found {
                        Some(a) => {
                            if trace {
                                traced.push((slot, shown, a));
                            } else {
                                self.mem()[slot..slot + 8].copy_from_slice(&a.to_le_bytes());
                            }
                        }
                        None => pending.push((slot, idx)),
                    }
                }
                desc += 20;
            }
        }
        if !traced.is_empty() {
            let entries: Vec<(String, u64)> = traced.iter().map(|t| (t.1.clone(), t.2)).collect();
            let page = TracePage::new(&entries)?;
            for (k, t) in traced.iter().enumerate() {
                let a = page.addr(k);
                self.mem()[t.0..t.0 + 8].copy_from_slice(&a.to_le_bytes());
            }
            self._trace = Some(page);
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

    /// Fills the export table of a DLL: names, ordinals, forwarders resolved (to another
    /// package DLL, else to nativ64; an unresolvable forwarder is left out).
    fn resolve_exports(&mut self, pe: &Pe, ctx: &mut Ctx) -> Result<(), LoadError> {
        let dir = pe.dirs[DIR_EXPORT];
        let raw = exports::parse(self.mem(), dir.rva, dir.size)?;
        let base = self.base();
        let mut table = Exports::default();
        let addr = |i: usize| raw.ordinal_base.wrapping_add(i as u32) as u16;
        for (i, f) in raw.funcs.iter().enumerate() {
            if let RawTarget::Rva(r) = f {
                table.ordinals.insert(addr(i), base + *r as u64);
            }
        }
        for (n, i) in &raw.names {
            if let Some(&a) = table.ordinals.get(&addr(*i)) {
                table.names.insert(n.clone(), a);
            }
        }
        for (i, f) in raw.funcs.iter().enumerate() {
            let RawTarget::Forward(fwd) = f else { continue };
            if let Some(a) = ctx.resolve_forward(fwd, &self.name, &table)? {
                table.ordinals.insert(addr(i), a);
                for (n, j) in &raw.names {
                    if *j == i {
                        table.names.insert(n.clone(), a);
                    }
                }
            }
        }
        self.exports = Arc::new(table);
        Ok(())
    }

    /// Validates the TLS directory and writes its TLS index (0 for the main image, then 1, 2...
    /// for the DLLs); the image is still writable.
    fn setup_tls(&mut self, pe: &Pe, ctx: &mut Ctx, is_dll: bool) -> Result<(), LoadError> {
        let dir = pe.dirs[DIR_TLS];
        if dir.rva == 0 || dir.size == 0 {
            return Ok(());
        }
        let (info, index_rva) = TlsInfo::parse(self.mem(), self.base(), dir.rva, dir.size)?;
        let index = if is_dll {
            if ctx.next_tls >= MAX_TLS_IMAGES {
                return Err(LoadError::BadTls("too many images with static TLS"));
            }
            ctx.next_tls += 1;
            ctx.next_tls - 1
        } else {
            0
        };
        self.mem()[index_rva..index_rva + 4].copy_from_slice(&(index as u32).to_le_bytes());
        self.tls = Some((index, info));
        Ok(())
    }

    /// Registers the package DLLs with nativ64 (`GetModuleHandle`, `GetProcAddress`).
    fn register_dlls(&self) {
        for d in &self.dlls {
            let ex = d.exports.clone();
            nativ64::process::register_module(
                &d.name,
                &d.path,
                d.base as usize,
                Arc::new(move |r| ex.get(r).map(|a| a as usize)),
            );
        }
    }

    /// Runs the package DLLs' initialisation and the entry point as an EXE
    /// `extern "win64" fn() -> u32` on a new thread with a 64 MiB stack, a TEB/PEB and static
    /// TLS; returns its result. Before the entry, each DLL in dependency order gets its TLS
    /// callbacks and `DllMain(base, DLL_PROCESS_ATTACH, 0)` (FALSE is an error), then the main
    /// image's TLS callbacks run. A guest crash is the caller's business (see [`crate::fault`]).
    pub fn run_main(&self) -> Result<u32, LoadError> {
        if self.entry_rva == 0 {
            return Err(LoadError::NoEntryPoint);
        }
        self.register_dlls();
        let (base, entry) = (self.base(), self.entry_point());
        // (name, base, DllMain, TLS) per DLL
        let dlls: Vec<(String, u64, Option<u64>, Option<TlsInfo>)> = self
            .dlls
            .iter()
            .map(|d| {
                let main = (d.entry_rva != 0).then(|| d.entry_point());
                (
                    d.name.clone(),
                    d.base(),
                    main,
                    d.tls.as_ref().map(|t| t.1.clone()),
                )
            })
            .collect();
        let mut images: Vec<(usize, TlsInfo)> =
            self.dlls.iter().filter_map(|d| d.tls.clone()).collect();
        let tls = self.tls.clone();
        images.extend(tls.clone());
        let t = std::thread::Builder::new()
            .name("nativ64-main".into())
            .stack_size(64 << 20)
            .spawn(move || -> Result<u32, LoadError> {
                crate::fault::setup_thread();
                let (lo, hi) = current_stack()?;
                crate::teb::set_image_base(base)?;
                let teb = crate::teb::install_for_current_thread(hi, lo)?;
                let mut block = (!images.is_empty()).then(|| unsafe { ThreadTls::new(&images) });
                if let Some(b) = block.as_mut() {
                    teb.set_tls_pointer(b.array());
                }
                for (name, dbase, main, info) in &dlls {
                    if let Some(i) = info {
                        unsafe { tls::run_callbacks(i, *dbase) };
                    }
                    if let Some(m) = main {
                        // SAFETY: the entry point of a mapped DLL is a DllMain.
                        let f: extern "win64" fn(u64, u32, usize) -> i32 =
                            unsafe { std::mem::transmute(*m as usize) };
                        if f(*dbase, 1, 0) == 0 {
                            return Err(LoadError::DllInitFailed(name.clone()));
                        }
                    }
                }
                if let Some((_, i)) = &tls {
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

/// Module file name as a map key: lowercase, `.dll` added when there is no extension.
fn module_key(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    if n.contains('.') {
        n
    } else {
        format!("{n}.dll")
    }
}

/// State shared while mapping an image and the package DLLs it needs.
struct Ctx<'a> {
    opts: &'a LoadOptions,
    /// Files of the search directories by lowercase name (earlier directories win).
    files: HashMap<String, PathBuf>,
    /// Module key to its index in `dlls`; `None` while it is being loaded (cycle guard).
    state: HashMap<String, Option<usize>>,
    /// Loaded DLLs, dependencies first.
    dlls: Vec<Image>,
    next_tls: usize,
}

impl<'a> Ctx<'a> {
    fn new(opts: &'a LoadOptions) -> Ctx<'a> {
        let mut files = HashMap::new();
        for d in &opts.search_dirs {
            let Ok(rd) = std::fs::read_dir(d) else {
                continue;
            };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                if n.ends_with(".dll") && e.path().is_file() {
                    files.entry(n).or_insert_with(|| e.path());
                }
            }
        }
        Ctx {
            opts,
            files,
            state: HashMap::new(),
            dlls: Vec::new(),
            next_tls: 1,
        }
    }

    /// Index of the package DLL for `module` in `dlls`, mapping it (and its dependencies) on
    /// first use. `None`: nativ64 provides the module (no file, forced native, or still loading).
    fn ensure(&mut self, module: &str) -> Result<Option<usize>, LoadError> {
        let key = module_key(module);
        if self
            .opts
            .native_modules
            .iter()
            .any(|n| module_key(n) == key)
        {
            return Ok(None);
        }
        if let Some(st) = self.state.get(&key) {
            return Ok(*st);
        }
        let Some(path) = self.files.get(&key).cloned() else {
            return Ok(None);
        };
        self.state.insert(key.clone(), None);
        let wrap = |e| LoadError::Dll(key.clone(), Box::new(e));
        let data = std::fs::read(&path)
            .map_err(|e| wrap(LoadError::Os("read", e.raw_os_error().unwrap_or(0))))?;
        let img = Image::load_one(&data, self, None, Some((key.clone(), path))).map_err(wrap)?;
        self.dlls.push(img);
        let idx = self.dlls.len() - 1;
        self.state.insert(key, Some(idx));
        Ok(Some(idx))
    }

    /// Resolves an export forwarder `module.name` / `module.#ordinal` of the DLL `own` (whose
    /// table so far is `own_table`): another package DLL, else nativ64, else `None`.
    fn resolve_forward(
        &mut self,
        fwd: &str,
        own: &str,
        own_table: &Exports,
    ) -> Result<Option<u64>, LoadError> {
        let Some((module, func)) = fwd.rsplit_once('.') else {
            return Ok(None);
        };
        let ordinal = func.strip_prefix('#').and_then(|o| o.parse::<u16>().ok());
        let r = match ordinal {
            Some(o) => ExportRef::Ordinal(o),
            None => ExportRef::Name(func),
        };
        if module_key(module) == own {
            return Ok(own_table.get(r));
        }
        if let Some(d) = self.ensure(module)? {
            return Ok(self.dlls[d].exports.get(r));
        }
        let m = module_key(module);
        Ok(match r {
            ExportRef::Ordinal(o) => nativ64::lookup_ordinal(&m, o),
            ExportRef::Name(n) => nativ64::lookup(&m, n).or_else(|| nativ64_gpu::lookup(&m, n)),
        }
        .map(|e| e.addr as u64))
    }
}
