//! Export directory parsing and the resolved export table of a mapped DLL.

use crate::pe::{rd_u16, rd_u32};
use crate::LoadError;
use nativ64::process::ExportRef;
use std::collections::HashMap;

/// One slot of the export address table, before forwarders are resolved.
pub enum RawTarget {
    Unused,
    Rva(u32),
    /// `module.function` or `module.#ordinal`.
    Forward(String),
}

pub struct RawExports {
    /// The directory's `Base` (ordinal of the first slot).
    pub ordinal_base: u32,
    pub funcs: Vec<RawTarget>,
    /// Exported name and index into `funcs`.
    pub names: Vec<(String, usize)>,
}

fn cstr(mem: &[u8], rva: u32) -> Result<String, LoadError> {
    let tail = mem
        .get(rva as usize..)
        .ok_or(LoadError::BadExports("string out of bounds"))?;
    let n = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or(LoadError::BadExports("string not terminated"))?;
    Ok(String::from_utf8_lossy(&tail[..n]).into_owned())
}

/// Parses the export directory at `rva`/`size` of the mapped image `mem`; every array and
/// string must lie inside the image. No directory gives an empty table.
pub fn parse(mem: &[u8], rva: u32, size: u32) -> Result<RawExports, LoadError> {
    let empty = RawExports {
        ordinal_base: 1,
        funcs: Vec::new(),
        names: Vec::new(),
    };
    if rva == 0 || size == 0 {
        return Ok(empty);
    }
    let d = rva as usize;
    if size < 40 || d.checked_add(40).is_none_or(|e| e > mem.len()) {
        return Err(LoadError::BadExports("directory out of bounds"));
    }
    let end = d.saturating_add(size as usize);
    let rd = |o: usize| rd_u32(mem, d + o, "export directory");
    let ordinal_base = rd(16)?;
    let nfuncs = rd(20)? as usize;
    let nnames = rd(24)? as usize;
    let (funcs_rva, names_rva, ords_rva) = (rd(28)? as usize, rd(32)? as usize, rd(36)? as usize);
    if nfuncs > 0x1_0000 || nnames > 0x1_0000 {
        return Err(LoadError::BadExports("too many exports"));
    }
    let fits = |at: usize, len: usize| at.checked_add(len).is_some_and(|e| e <= mem.len());
    if !fits(funcs_rva, nfuncs * 4) {
        return Err(LoadError::BadExports("function table out of bounds"));
    }
    if !fits(names_rva, nnames * 4) || !fits(ords_rva, nnames * 2) {
        return Err(LoadError::BadExports("name tables out of bounds"));
    }
    let mut funcs = Vec::with_capacity(nfuncs);
    for i in 0..nfuncs {
        let r = rd_u32(mem, funcs_rva + i * 4, "export function table")?;
        funcs.push(if r == 0 {
            RawTarget::Unused
        } else if (r as usize) >= d && (r as usize) < end {
            RawTarget::Forward(cstr(mem, r)?)
        } else if (r as usize) >= mem.len() {
            return Err(LoadError::BadExports("function RVA out of bounds"));
        } else {
            RawTarget::Rva(r)
        });
    }
    let mut names = Vec::with_capacity(nnames);
    for i in 0..nnames {
        let n = rd_u32(mem, names_rva + i * 4, "export name table")?;
        let idx = rd_u16(mem, ords_rva + i * 2, "export ordinal table")? as usize;
        if idx >= nfuncs {
            return Err(LoadError::BadExports("name ordinal out of range"));
        }
        names.push((cstr(mem, n)?, idx));
    }
    Ok(RawExports {
        ordinal_base,
        funcs,
        names,
    })
}

/// The final export addresses of a DLL (forwarders resolved).
#[derive(Default)]
pub struct Exports {
    pub names: HashMap<String, u64>,
    pub ordinals: HashMap<u16, u64>,
}

impl Exports {
    pub fn get(&self, r: ExportRef<'_>) -> Option<u64> {
        match r {
            ExportRef::Name(n) => self.names.get(n).copied(),
            ExportRef::Ordinal(o) => self.ordinals.get(&o).copied(),
        }
    }
}
