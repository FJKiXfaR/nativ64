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
