//! nativ64-gpu: the graphics modules a title imports (Xbox Direct3D 12 and friends),
//! kept apart from the host APIs of `nativ64`. Exports share nativ64's `Export` registry type.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("nativ64-gpu supports Linux x86_64 only");

pub mod d3d12_xs;
pub mod xcrdapi;
pub mod xmem;
pub mod xobj;

use nativ64::{normalize_module, Export};

/// One table per Windows module file. Add a new module file here.
pub static TABLES: &[&[Export]] = &[d3d12_xs::EXPORTS, xcrdapi::EXPORTS, xmem::EXPORTS];

/// Every registered graphics export.
pub fn exports() -> impl Iterator<Item = &'static Export> {
    TABLES.iter().flat_map(|t| t.iter())
}

pub fn lookup(module: &str, name: &str) -> Option<&'static Export> {
    let m = normalize_module(module);
    exports().find(|e| e.module == m && e.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_works() {
        let e = lookup("D3D12_XS.dll", "D3D12ConfigureVirtualMemoryX").unwrap();
        assert_eq!(e.status, nativ64::Status::Native);
        assert!(lookup("d3d12_xs", "Nope").is_none());
        assert!(lookup("kernel32", "D3D12ConfigureVirtualMemoryX").is_none());
    }
}
