//! vn-win32: Windows host APIs (kernel32, api-ms-win-core-*, ...) reimplemented natively
//! on Linux, independent of the compatibility layer that loads the game.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("vn-win32 supports Linux x86_64 only");

#[macro_use]
mod registry;

pub mod api_ms_win_core_errorhandling;
pub mod api_ms_win_core_processthreads;
pub mod api_ms_win_core_profile;
pub mod api_ms_win_core_synch;
pub mod api_ms_win_core_sysinfo;

pub use registry::{lookup, normalize_module, vn_win32_lookup, Export, Status};

/// One table per Windows module file. Add a new module file here.
pub static TABLES: &[&[Export]] = &[
    api_ms_win_core_errorhandling::EXPORTS,
    api_ms_win_core_processthreads::EXPORTS,
    api_ms_win_core_profile::EXPORTS,
    api_ms_win_core_synch::EXPORTS,
    api_ms_win_core_sysinfo::EXPORTS,
];

/// Every registered export (each function appears once per module name it is known under).
pub fn exports() -> impl Iterator<Item = &'static Export> {
    TABLES.iter().flat_map(|t| t.iter())
}
