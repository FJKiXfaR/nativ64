//! PE32+ header parsing, bounds-checked.

use crate::LoadError;

pub const MACHINE_AMD64: u16 = 0x8664;
pub const DIR_EXPORT: usize = 0;
pub const DIR_IMPORT: usize = 1;
pub const DIR_BASERELOC: usize = 5;
pub const DIR_TLS: usize = 9;

pub const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const SCN_MEM_READ: u32 = 0x4000_0000;
pub const SCN_MEM_WRITE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataDir {
    pub rva: u32,
    pub size: u32,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

impl Section {
    /// Bytes the section occupies in memory (VirtualSize, or the raw size when it is 0).
    pub fn mem_size(&self) -> u32 {
        if self.virtual_size != 0 {
            self.virtual_size
        } else {
            self.raw_size
        }
    }
}

#[derive(Debug)]
pub struct Pe<'a> {
    pub data: &'a [u8],
    pub image_base: u64,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub entry_rva: u32,
    pub sections: Vec<Section>,
    pub dirs: [DataDir; 16],
}

pub fn rd_u16(d: &[u8], off: usize, what: &'static str) -> Result<u16, LoadError> {
    d.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(LoadError::Truncated(what))
}

pub fn rd_u32(d: &[u8], off: usize, what: &'static str) -> Result<u32, LoadError> {
    d.get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or(LoadError::Truncated(what))
}

pub fn rd_u64(d: &[u8], off: usize, what: &'static str) -> Result<u64, LoadError> {
    d.get(off..off + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or(LoadError::Truncated(what))
}

impl<'a> Pe<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, LoadError> {
        if data.len() < 2 {
            return Err(LoadError::Truncated("DOS header"));
        }
        if &data[..2] != b"MZ" {
            return Err(LoadError::BadDosMagic);
        }
        let nt = rd_u32(data, 0x3c, "DOS header")? as usize;
        let sig = data
            .get(nt..nt.saturating_add(4))
            .ok_or(LoadError::Truncated("NT signature"))?;
        if sig != b"PE\0\0" {
            return Err(LoadError::BadNtSignature);
        }
        let fh = nt + 4;
        let machine = rd_u16(data, fh, "file header")?;
        let nsections = rd_u16(data, fh + 2, "file header")? as usize;
        let opt_size = rd_u16(data, fh + 16, "file header")? as usize;
        let opt = fh + 20;
        let magic = rd_u16(data, opt, "optional header")?;
        if magic != 0x20b {
            return Err(LoadError::NotPe32Plus(magic));
        }
        if machine != MACHINE_AMD64 {
            return Err(LoadError::UnsupportedMachine(machine));
        }
        if opt_size < 112 {
            return Err(LoadError::BadHeader("optional header too small"));
        }
        if data.len() < opt + opt_size {
            return Err(LoadError::Truncated("optional header"));
        }
        let entry_rva = rd_u32(data, opt + 16, "optional header")?;
        let image_base = rd_u64(data, opt + 24, "optional header")?;
        let size_of_image = rd_u32(data, opt + 56, "optional header")?;
        let size_of_headers = rd_u32(data, opt + 60, "optional header")?;
        let ndirs = (rd_u32(data, opt + 108, "optional header")? as usize).min(16);
        if (112 + ndirs * 8) > opt_size {
            return Err(LoadError::BadHeader(
                "data directories exceed optional header",
            ));
        }
        if size_of_headers > size_of_image || size_of_headers as usize > data.len() {
            return Err(LoadError::BadHeader("SizeOfHeaders out of bounds"));
        }
        let mut dirs = [DataDir::default(); 16];
        for (i, d) in dirs.iter_mut().enumerate().take(ndirs) {
            let o = opt + 112 + i * 8;
            d.rva = rd_u32(data, o, "data directory")?;
            d.size = rd_u32(data, o + 4, "data directory")?;
        }
        let table = opt + opt_size;
        let mut sections = Vec::with_capacity(nsections);
        for i in 0..nsections {
            let o = table + i * 40;
            let raw = data
                .get(o..o + 40)
                .ok_or(LoadError::Truncated("section table"))?;
            let end = raw[..8].iter().position(|&b| b == 0).unwrap_or(8);
            let name = String::from_utf8_lossy(&raw[..end]).into_owned();
            let u32_at = |p: usize| u32::from_le_bytes(raw[p..p + 4].try_into().unwrap());
            let s = Section {
                name,
                virtual_size: u32_at(8),
                virtual_address: u32_at(12),
                raw_size: u32_at(16),
                raw_offset: u32_at(20),
                characteristics: u32_at(36),
            };
            let mem_end = s.virtual_address as u64 + s.mem_size() as u64;
            let raw_end = s.raw_offset as u64 + s.raw_size as u64;
            if mem_end > size_of_image as u64 || raw_end > data.len() as u64 {
                return Err(LoadError::SectionOutOfBounds(s.name));
            }
            if !s.virtual_address.is_multiple_of(4096) {
                return Err(LoadError::UnalignedSection(s.name));
            }
            sections.push(s);
        }
        Ok(Pe {
            data,
            image_base,
            size_of_image,
            size_of_headers,
            entry_rva,
            sections,
            dirs,
        })
    }
}
