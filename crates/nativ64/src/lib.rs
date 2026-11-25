//! nativ64: Windows host APIs (kernel32, api-ms-win-core-*, ...) reimplemented natively
//! on Linux, independent of the compatibility layer that loads the game.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("nativ64 supports Linux x86_64 only");

#[macro_use]
mod registry;

pub mod api_ms_win_core_errorhandling;
pub mod api_ms_win_core_fibers;
pub mod api_ms_win_core_handle;
pub mod api_ms_win_core_interlocked;
pub mod api_ms_win_core_libraryloader;
pub mod api_ms_win_core_processenvironment;
pub mod api_ms_win_core_processthreads;
pub mod api_ms_win_core_profile;
pub mod api_ms_win_core_registry;
pub mod api_ms_win_core_synch;
pub mod api_ms_win_core_sysinfo;
pub mod api_ms_win_crt_convert;
pub mod api_ms_win_crt_environment;
pub mod api_ms_win_crt_heap;
pub mod api_ms_win_crt_locale;
pub mod api_ms_win_crt_math;
pub mod api_ms_win_crt_runtime;
pub mod api_ms_win_crt_stdio;
pub mod api_ms_win_crt_string;
pub mod api_ms_win_crt_time;
pub mod api_ms_win_crt_utility;

mod handles;
pub mod process;
mod slots;
pub mod vcruntime140;

pub use api_ms_win_crt_runtime::{nativ64_set_command_line, set_command_line};
pub use registry::{lookup, nativ64_lookup, normalize_module, Export, Status};

/// One table per Windows module file. Add a new module file here.
pub static TABLES: &[&[Export]] = &[
    api_ms_win_core_errorhandling::EXPORTS,
    api_ms_win_core_fibers::EXPORTS,
    api_ms_win_core_handle::EXPORTS,
    api_ms_win_core_interlocked::EXPORTS,
    api_ms_win_core_libraryloader::EXPORTS,
    api_ms_win_core_registry::EXPORTS,
    api_ms_win_core_processenvironment::EXPORTS,
    api_ms_win_core_processthreads::EXPORTS,
    api_ms_win_core_processthreads::ALIASES,
    api_ms_win_core_profile::EXPORTS,
    api_ms_win_core_synch::EXPORTS,
    api_ms_win_core_sysinfo::EXPORTS,
    api_ms_win_crt_convert::EXPORTS,
    api_ms_win_crt_environment::EXPORTS,
    api_ms_win_crt_heap::EXPORTS,
    api_ms_win_crt_locale::EXPORTS,
    api_ms_win_crt_math::EXPORTS,
    api_ms_win_crt_runtime::EXPORTS,
    api_ms_win_crt_stdio::EXPORTS,
    api_ms_win_crt_string::EXPORTS,
    api_ms_win_crt_time::EXPORTS,
    api_ms_win_crt_utility::EXPORTS,
    vcruntime140::EXPORTS,
];

/// Every registered export (each function appears once per module name it is known under).
pub fn exports() -> impl Iterator<Item = &'static Export> {
    TABLES.iter().flat_map(|t| t.iter())
}
