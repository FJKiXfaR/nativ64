//! The DXBC container around a shader: part table, signatures (`ISG1`/`OSG1`/`PSG1`), the
//! `DXIL` part's header and the `XDAT` entry table that the console compiler puts in it.
//!
//! Layout and what was checked on real blobs: `docs/shaders.md`, section Container. Parts of an
//! unknown kind are kept in the table and otherwise ignored. The container digest is not checked.

use super::ShaderError;

/// Most parts a container may hold (the corpus has 7).
const MAX_PARTS: u32 = 64;
/// Most `XDAT` entries accepted (the corpus has 3).
const MAX_XDAT: u32 = 16;
/// Most signature elements accepted (D3D12 allows 32 per stage).
const MAX_SIGNATURE: u32 = 64;
/// Bytes before the part offsets.
const HEADER: usize = 0x20;
/// Bytes of a part header (fourcc, size).
const PART_HEADER: usize = 8;
/// Bytes of one `SG1` signature element.
const SG1_ELEMENT: usize = 32;
/// Offset of the `XDAT` magic in the `DXIL` part.
const XDAT_AT: usize = 0x18;

pub(super) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// One part of the container: its fourcc and where its data is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub fourcc: [u8; 4],
    /// Offset of the part header in the blob.
    pub at: usize,
    /// Size of the part data.
    pub size: usize,
}

/// A parsed DXBC container, borrowing the blob.
#[derive(Debug)]
pub struct Container<'a> {
    bytes: &'a [u8],
    pub digest: [u8; 16],
    /// Total size the header declares.
    pub total_size: usize,
    pub parts: Vec<Part>,
}

impl<'a> Container<'a> {
    /// Parses the header and the part table. Every part must lie inside the declared total size,
    /// after the part table; a blob longer than the declared size is accepted (trailing bytes
    /// ignored), a shorter one is truncated.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ShaderError> {
        if bytes.len() < HEADER {
            return Err(ShaderError::Truncated(format!(
                "container header: {} bytes, need {HEADER}",
                bytes.len()
            )));
        }
        if &bytes[0..4] != b"DXBC" {
            return Err(ShaderError::BadMagic("DXBC"));
        }
        let (major, minor) = (
            u16::from_le_bytes([bytes[0x14], bytes[0x15]]),
            u16::from_le_bytes([bytes[0x16], bytes[0x17]]),
        );
        if (major, minor) != (1, 0) {
            return Err(ShaderError::Unsupported(format!(
                "container version {major}.{minor}"
            )));
        }
        let total = u32_at(bytes, 0x18).expect("header read") as usize;
        if total < HEADER || total > bytes.len() {
            return Err(ShaderError::Truncated(format!(
                "container declares {total} bytes, blob has {}",
                bytes.len()
            )));
        }
        let count = u32_at(bytes, 0x1c).expect("header read");
        if count > MAX_PARTS {
            return Err(ShaderError::Unsupported(format!("{count} parts")));
        }
        let table_end = HEADER + 4 * count as usize;
        if table_end > total {
            return Err(ShaderError::BadOffset(format!(
                "part table ends at {table_end:#x}, past the {total:#x} bytes of the container"
            )));
        }
        let mut parts = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let at = u32_at(bytes, HEADER + 4 * i).expect("table read") as usize;
            if at < table_end || at + PART_HEADER > total {
                return Err(ShaderError::BadOffset(format!(
                    "part {i} at {at:#x} (table ends at {table_end:#x}, container {total:#x})"
                )));
            }
            let fourcc: [u8; 4] = bytes[at..at + 4].try_into().expect("4 bytes");
            let size = u32_at(bytes, at + 4).expect("part read") as usize;
            if size > total - at - PART_HEADER {
                return Err(ShaderError::BadOffset(format!(
                    "part {i} '{}' at {at:#x} has {size:#x} data bytes, past the container end {total:#x}",
                    fourcc_str(&fourcc)
                )));
            }
            parts.push(Part { fourcc, at, size });
        }
        Ok(Container {
            bytes: &bytes[..total],
            digest: bytes[4..0x14].try_into().expect("16 bytes"),
            total_size: total,
            parts,
        })
    }

    /// The data of the first part with this fourcc.
    pub fn part(&self, fourcc: &[u8; 4]) -> Option<&'a [u8]> {
        let p = self.parts.iter().find(|p| &p.fourcc == fourcc)?;
        Some(&self.bytes[p.at + PART_HEADER..p.at + PART_HEADER + p.size])
    }

    /// A signature part (`ISG1`, `OSG1`, `PSG1`); `None` if absent.
    pub fn signature(&self, fourcc: &[u8; 4]) -> Result<Option<Signature>, ShaderError> {
        self.part(fourcc)
            .map(|d| Signature::parse(fourcc, d))
            .transpose()
    }

    /// The `DXIL` part's header and `XDAT` table.
    pub fn dxil(&self) -> Result<Dxil<'a>, ShaderError> {
        let data = self
            .part(b"DXIL")
            .ok_or(ShaderError::Missing("DXIL part"))?;
        Dxil::parse(data)
    }
}

/// A fourcc as text (non-printable bytes as `?`).
pub fn fourcc_str(f: &[u8; 4]) -> String {
    f.iter()
        .map(|&c| {
            if (0x20..0x7f).contains(&c) {
                c as char
            } else {
                '?'
            }
        })
        .collect()
}

/// The program kind in the `DXIL` header's version dword (D3D shader kinds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKind {
    Pixel,
    Vertex,
    Geometry,
    Hull,
    Domain,
    Compute,
    Mesh,
    Amplification,
    Other(u16),
}

impl ProgramKind {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Pixel,
            1 => Self::Vertex,
            2 => Self::Geometry,
            3 => Self::Hull,
            4 => Self::Domain,
            5 => Self::Compute,
            13 => Self::Mesh,
            14 => Self::Amplification,
            n => Self::Other(n),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Pixel => "pixel".into(),
            Self::Vertex => "vertex".into(),
            Self::Geometry => "geometry".into(),
            Self::Hull => "hull".into(),
            Self::Domain => "domain".into(),
            Self::Compute => "compute".into(),
            Self::Mesh => "mesh".into(),
            Self::Amplification => "amplification".into(),
            Self::Other(n) => format!("kind{n}"),
        }
    }
}

/// One `XDAT` entry: a kind and a range of the `DXIL` part data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XdatEntry {
    pub kind: u32,
    pub offset: usize,
    pub size: usize,
}

/// `XDAT` entry kind of a native program (`XBCH`).
pub const XDAT_NATIVE: u32 = 0x0d;

/// The `DXIL` part: the program header and the `XDAT` entries.
#[derive(Debug)]
pub struct Dxil<'a> {
    data: &'a [u8],
    pub major: u8,
    pub minor: u8,
    pub kind: ProgramKind,
    /// The part size in dwords the header declares.
    pub size_dwords: u32,
    /// DXIL version (`0x106` = 1.6).
    pub dxil_version: u32,
    pub bitcode_offset: u32,
    pub bitcode_size: u32,
    pub xdat: Vec<XdatEntry>,
}

impl<'a> Dxil<'a> {
    fn parse(data: &'a [u8]) -> Result<Self, ShaderError> {
        let word = |at| {
            u32_at(data, at).ok_or_else(|| {
                ShaderError::Truncated(format!(
                    "DXIL header: {} bytes, read at {at:#x}",
                    data.len()
                ))
            })
        };
        word(0x14)?; // the fixed header
        let version = word(0)?;
        let size_dwords = word(4)?;
        if data.get(8..12) != Some(b"DXIL") {
            return Err(ShaderError::BadMagic("DXIL"));
        }
        let (dxil_version, bitcode_offset, bitcode_size) = (word(0xc)?, word(0x10)?, word(0x14)?);
        // The bitcode offset counts from the "DXIL" magic (+8).
        if (bitcode_offset as usize)
            .checked_add(bitcode_size as usize)
            .and_then(|e| e.checked_add(8))
            .is_none_or(|e| e > data.len())
        {
            return Err(ShaderError::BadOffset(format!(
                "DXIL bitcode {bitcode_offset:#x}+{bitcode_size:#x} past the {:#x}-byte part",
                data.len()
            )));
        }
        let mut xdat = Vec::new();
        if data.get(XDAT_AT..XDAT_AT + 4) == Some(b"XDAT") {
            let count = word(XDAT_AT + 4)?;
            let table = word(XDAT_AT + 8)? as usize;
            if count > MAX_XDAT {
                return Err(ShaderError::Unsupported(format!("{count} XDAT entries")));
            }
            for i in 0..count as usize {
                let at = table + 12 * i;
                let (kind, offset, size) =
                    (word(at)?, word(at + 4)? as usize, word(at + 8)? as usize);
                if offset.checked_add(size).is_none_or(|e| e > data.len()) {
                    return Err(ShaderError::BadOffset(format!(
                        "XDAT entry {i} (kind {kind:#x}) at {offset:#x}+{size:#x} past the {:#x}-byte part",
                        data.len()
                    )));
                }
                xdat.push(XdatEntry { kind, offset, size });
            }
        }
        Ok(Dxil {
            data,
            major: ((version >> 4) & 0xf) as u8,
            minor: (version & 0xf) as u8,
            kind: ProgramKind::from_u16((version >> 16) as u16),
            size_dwords,
            dxil_version,
            bitcode_offset,
            bitcode_size,
            xdat,
        })
    }

    /// An entry's bytes.
    pub fn entry(&self, e: &XdatEntry) -> &'a [u8] {
        &self.data[e.offset..e.offset + e.size]
    }
}

/// One signature element (`SG1` layout: 32 bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigElement {
    pub stream: u32,
    pub name: String,
    pub index: u32,
    /// D3D system value (0 = none, 1 = position, 6 = vertex id ...).
    pub system_value: u32,
    /// D3D register component type (1 uint, 2 sint, 3 float).
    pub component_type: u32,
    pub register: u32,
    pub mask: u8,
    pub rw_mask: u8,
    pub min_precision: u32,
}

/// A signature part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub elements: Vec<SigElement>,
}

impl Signature {
    fn parse(fourcc: &[u8; 4], d: &[u8]) -> Result<Self, ShaderError> {
        let what = fourcc_str(fourcc);
        let short = |at: usize| {
            ShaderError::Truncated(format!("{what} part: {} bytes, read at {at:#x}", d.len()))
        };
        let count = u32_at(d, 0).ok_or_else(|| short(0))?;
        let start = u32_at(d, 4).ok_or_else(|| short(4))? as usize;
        if count > MAX_SIGNATURE {
            return Err(ShaderError::Unsupported(format!(
                "{what}: {count} elements"
            )));
        }
        let mut elements = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let at = start + SG1_ELEMENT * i;
            if at + SG1_ELEMENT > d.len() {
                return Err(ShaderError::BadOffset(format!(
                    "{what}: element {i} at {at:#x} past the {:#x}-byte part",
                    d.len()
                )));
            }
            let w = |k: usize| u32_at(d, at + 4 * k).expect("bounds checked");
            let name_at = w(1) as usize;
            let name = d
                .get(name_at..)
                .and_then(|s| s.iter().position(|&c| c == 0).map(|n| &s[..n]))
                .and_then(|s| std::str::from_utf8(s).ok())
                .ok_or_else(|| {
                    ShaderError::BadOffset(format!(
                        "{what}: element {i} name at {name_at:#x} is not a terminated string"
                    ))
                })?;
            elements.push(SigElement {
                stream: w(0),
                name: name.to_string(),
                index: w(2),
                system_value: w(3),
                component_type: w(4),
                register: w(5),
                mask: d[at + 24],
                rw_mask: d[at + 25],
                min_precision: w(7),
            });
        }
        Ok(Signature { elements })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn put32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// A signature part body of `(name, index, system value, register, mask)` elements.
    pub fn sig(elems: &[(&str, u32, u32, u32, u8)]) -> Vec<u8> {
        let mut v = Vec::new();
        put32(&mut v, elems.len() as u32);
        put32(&mut v, 8);
        let mut name_at = 8 + 32 * elems.len();
        let mut names = Vec::new();
        for &(name, index, sv, reg, mask) in elems {
            for x in [0, name_at as u32, index, sv, 3, reg] {
                put32(&mut v, x);
            }
            v.extend_from_slice(&[mask, 0, 0, 0]);
            put32(&mut v, 0);
            names.extend_from_slice(name.as_bytes());
            names.push(0);
            name_at += name.len() + 1;
        }
        v.extend(names);
        v
    }

    /// A container of `(fourcc, data)` parts, offsets laid out back to back.
    pub fn container(parts: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut v = b"DXBC".to_vec();
        v.extend_from_slice(&[0xab; 16]);
        v.extend_from_slice(&[1, 0, 0, 0]);
        v.extend_from_slice(&[0; 8]); // size, count: patched below
        let mut at = HEADER + 4 * parts.len();
        for (_, d) in parts {
            put32(&mut v, at as u32);
            at += PART_HEADER + d.len();
        }
        for (f, d) in parts {
            v.extend_from_slice(*f);
            put32(&mut v, d.len() as u32);
            v.extend_from_slice(d);
        }
        let total = v.len() as u32;
        v[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
        v[0x1c..0x20].copy_from_slice(&(parts.len() as u32).to_le_bytes());
        v
    }

    /// A `DXIL` part with `kind`, and XDAT entries of `(kind, data)` after the header.
    pub fn dxil(kind: u16, entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut v = Vec::new();
        put32(&mut v, (kind as u32) << 16 | 0x66);
        put32(&mut v, 0); // size in dwords, patched
        v.extend_from_slice(b"DXIL");
        for x in [0x106, 0, 0] {
            put32(&mut v, x);
        }
        v.extend_from_slice(b"XDAT");
        put32(&mut v, entries.len() as u32);
        put32(&mut v, 0x24);
        let mut at = 0x24 + 12 * entries.len();
        for (k, d) in entries {
            for x in [*k, at as u32, d.len() as u32] {
                put32(&mut v, x);
            }
            at += d.len();
        }
        for (_, d) in entries {
            v.extend_from_slice(d);
        }
        let dw = (v.len() / 4) as u32;
        v[4..8].copy_from_slice(&dw.to_le_bytes());
        // Bitcode: the last four bytes.
        let (off, len) = (v.len() as u32 - 8 - 4, 4u32);
        v[0x10..0x14].copy_from_slice(&off.to_le_bytes());
        v[0x14..0x18].copy_from_slice(&len.to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v
    }

    #[test]
    fn parses_parts_and_ignores_unknown_kinds() {
        let blob = container(&[
            (b"SFI0", vec![0; 8]),
            (b"ZZZZ", vec![1, 2, 3]),
            (b"DXIL", dxil(1, &[(0x12, vec![7; 16])])),
        ]);
        let c = Container::parse(&blob).unwrap();
        assert_eq!(c.parts.len(), 3);
        assert_eq!(c.part(b"ZZZZ"), Some(&[1u8, 2, 3][..]));
        assert_eq!(c.part(b"NONE"), None);
        assert_eq!(c.digest, [0xab; 16]);
        let d = c.dxil().unwrap();
        assert_eq!((d.major, d.minor, d.kind), (6, 6, ProgramKind::Vertex));
        assert_eq!(d.dxil_version, 0x106);
        assert_eq!(d.xdat.len(), 1);
        assert_eq!(d.entry(&d.xdat[0]), &[7u8; 16][..]);
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let mut blob = container(&[(b"SFI0", vec![0; 8])]);
        let n = blob.len();
        blob.extend_from_slice(&[9; 5]);
        assert_eq!(Container::parse(&blob).unwrap().total_size, n);
    }

    #[test]
    fn refuses_bad_containers() {
        let good = container(&[(b"SFI0", vec![0; 8]), (b"ZZZZ", vec![0; 4])]);
        assert!(matches!(
            Container::parse(&good[..10]),
            Err(ShaderError::Truncated(_))
        ));
        let mut b = good.clone();
        b[0] = b'X';
        assert!(matches!(
            Container::parse(&b),
            Err(ShaderError::BadMagic("DXBC"))
        ));
        let mut b = good.clone();
        b[0x14] = 2;
        assert!(matches!(
            Container::parse(&b),
            Err(ShaderError::Unsupported(_))
        ));
        // Declared size past the blob, and a blob cut short.
        assert!(matches!(
            Container::parse(&good[..good.len() - 1]),
            Err(ShaderError::Truncated(_))
        ));
        let mut b = good.clone();
        b[0x1c] = 200;
        assert!(matches!(
            Container::parse(&b),
            Err(ShaderError::Unsupported(_))
        ));
        let mut b = good.clone();
        b[0x1c] = 12; // table runs past the end
        assert!(matches!(
            Container::parse(&b),
            Err(ShaderError::BadOffset(_))
        ));
        // Part offsets: inside the table, past the end.
        for bad in [4u32, 0x1000] {
            let mut b = good.clone();
            b[0x24..0x28].copy_from_slice(&bad.to_le_bytes());
            assert!(matches!(
                Container::parse(&b),
                Err(ShaderError::BadOffset(_))
            ));
        }
        // Part size past the container.
        let mut b = good.clone();
        let at = u32_at(&b, 0x20).unwrap() as usize;
        b[at + 4..at + 8].copy_from_slice(&0x1000u32.to_le_bytes());
        assert!(matches!(
            Container::parse(&b),
            Err(ShaderError::BadOffset(_))
        ));
    }

    #[test]
    fn refuses_bad_dxil() {
        let part = |d: Vec<u8>| container(&[(b"DXIL", d)]);
        let run = |d: Vec<u8>| {
            let blob = part(d);
            Container::parse(&blob).unwrap().dxil().err()
        };
        assert!(matches!(
            Container::parse(&container(&[])).unwrap().dxil(),
            Err(ShaderError::Missing(_))
        ));
        assert!(matches!(run(vec![0; 8]), Some(ShaderError::Truncated(_))));
        let good = dxil(0, &[(0x12, vec![0; 16])]);
        let mut d = good.clone();
        d[8] = b'Q';
        assert!(matches!(run(d), Some(ShaderError::BadMagic("DXIL"))));
        let mut d = good.clone();
        d[0x10] = 0xff; // bitcode offset
        assert!(matches!(run(d), Some(ShaderError::BadOffset(_))));
        let mut d = good.clone();
        d[0x1c..0x20].copy_from_slice(&99u32.to_le_bytes()); // entry count
        assert!(matches!(run(d), Some(ShaderError::Unsupported(_))));
        let mut d = good.clone();
        d[0x28..0x2c].copy_from_slice(&0x1000u32.to_le_bytes()); // entry offset
        assert!(matches!(run(d), Some(ShaderError::BadOffset(_))));
        let mut d = good.clone();
        d[0x20..0x24].copy_from_slice(&0x1000u32.to_le_bytes()); // table past the part
        assert!(matches!(run(d), Some(ShaderError::Truncated(_))));
    }

    #[test]
    fn signatures() {
        let blob = container(&[
            (
                b"OSG1",
                sig(&[("SV_Position", 0, 1, 0, 0xf), ("TEXCOORD", 3, 0, 1, 7)]),
            ),
            (b"ISG1", vec![1, 0, 0]),
        ]);
        let c = Container::parse(&blob).unwrap();
        let s = c.signature(b"OSG1").unwrap().unwrap();
        assert_eq!(s.elements[0].name, "SV_Position");
        assert_eq!(s.elements[0].system_value, 1);
        assert_eq!(s.elements[1].name, "TEXCOORD");
        assert_eq!((s.elements[1].index, s.elements[1].register), (3, 1));
        assert_eq!(s.elements[1].mask, 7);
        assert_eq!(c.signature(b"PSG1").unwrap(), None);
        assert!(matches!(
            c.signature(b"ISG1"),
            Err(ShaderError::Truncated(_))
        ));
    }

    #[test]
    fn refuses_bad_signatures() {
        let good = sig(&[("A", 0, 0, 0, 1)]);
        let run = |d: Vec<u8>| {
            let blob = container(&[(b"ISG1", d)]);
            Container::parse(&blob).unwrap().signature(b"ISG1").err()
        };
        let mut d = good.clone();
        d[0] = 200; // element count
        assert!(matches!(run(d), Some(ShaderError::Unsupported(_))));
        let mut d = good.clone();
        d[0] = 3; // elements past the part
        assert!(matches!(run(d), Some(ShaderError::BadOffset(_))));
        let mut d = good.clone();
        d[12..16].copy_from_slice(&0x500u32.to_le_bytes()); // name offset
        assert!(matches!(run(d), Some(ShaderError::BadOffset(_))));
        let mut d = good.clone();
        let n = d.len();
        d[n - 1] = b'x'; // name without terminator
        assert!(matches!(run(d), Some(ShaderError::BadOffset(_))));
    }
}
