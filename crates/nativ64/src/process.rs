//! Process-wide facts the base registers: the main module (the image the base mapped).
//!
//! nativ64 does not load PE images, so the base tells it where the main image is and what its
//! path is. `GetModuleHandle(NULL)`, `GetModuleFileName(NULL, ...)` and the like read it here.

use std::sync::Mutex;

/// The main module as registered by the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainModule {
    pub base: usize,
    pub path: String,
}

static MAIN_MODULE: Mutex<Option<MainModule>> = Mutex::new(None);

/// Registers the main image: its base address (the `HMODULE`) and its path (UTF-8). Call it
/// once after mapping, before running the entry point. A later call replaces the earlier one.
pub fn set_main_module(base: usize, path: &str) {
    *MAIN_MODULE.lock().unwrap_or_else(|e| e.into_inner()) = Some(MainModule {
        base,
        path: path.to_string(),
    });
}

/// The registered main module, if any.
pub fn main_module() -> Option<MainModule> {
    MAIN_MODULE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// How an export is asked for: by name or by ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportRef<'a> {
    Name(&'a str),
    Ordinal(u16),
}

/// Looks an export up in a registered module; returns its address.
pub type ExportLookup = std::sync::Arc<dyn Fn(ExportRef<'_>) -> Option<usize> + Send + Sync>;

/// A guest DLL the base mapped and registered with [`register_module`].
#[derive(Clone)]
pub struct RegisteredModule {
    /// File name, lowercase (e.g. `msvcp140.dll`).
    pub name: String,
    pub path: String,
    pub base: usize,
    lookup: ExportLookup,
}

impl RegisteredModule {
    /// The address of an export of this module.
    pub fn export(&self, r: ExportRef<'_>) -> Option<usize> {
        (self.lookup)(r)
    }
}

static MODULES: Mutex<Vec<RegisteredModule>> = Mutex::new(Vec::new());

/// Registers a mapped guest DLL so that `GetModuleHandle*` returns `base` for `name` and
/// `GetProcAddress(base, ...)` resolves through `lookup`. A module registered again under the
/// same name replaces the earlier one.
pub fn register_module(name: &str, path: &str, base: usize, lookup: ExportLookup) {
    let name = name.to_ascii_lowercase();
    let mut v = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|m| m.name != name);
    v.push(RegisteredModule {
        name,
        path: path.to_string(),
        base,
        lookup,
    });
}

/// Forgets every registered module (tests, or a base that unloads everything).
pub fn clear_modules() {
    MODULES.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A registered module by file name (case-insensitive; `.dll` optional).
pub fn registered_module(name: &str) -> Option<RegisteredModule> {
    let want = name.to_ascii_lowercase();
    let strip = |s: &str| s.strip_suffix(".dll").unwrap_or(s).to_string();
    let want = strip(&want);
    MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| strip(&m.name) == want)
        .cloned()
}

/// A registered module by base address.
pub fn registered_module_at(base: usize) -> Option<RegisteredModule> {
    MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| m.base == base)
        .cloned()
}
