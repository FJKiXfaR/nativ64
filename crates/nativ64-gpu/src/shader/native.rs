//! The native program inside a `DXIL` part's `XBCH` entry: zlib stream -> `FSSM` payload ->
//! per-stage records (code range, root layout and used-resource tables, register counts, stage
//! data). Layouts and what was checked on real blobs: `docs/shaders.md`, section Container.
//!
//! The vertex, pixel, compute and mesh records are decoded; other slots are reported as
//! present. Any offset or count outside the payload is an error, never a panic.

use super::container::u32_at;
use super::ShaderError;

/// Largest unpacked program accepted.
pub const MAX_UNPACKED: u64 = 16 << 20;
/// Bytes before the zlib stream in an `XBCH` entry.
const XBCH_HEADER: usize = 24;
/// Bytes before the payload in the unpacked data.
const FSSM_HEADER: usize = 16;
/// Absent record offset.
const ABSENT: u64 = 0xDEAD_BEEF_DEAD_BEEF;
/// Payload offsets of the record pointers: name, offset.
const SLOTS: [(&str, usize); 8] = [
    ("ls", 0x08),
    ("hs", 0x10),
    ("es", 0x18),
    ("gs", 0x20),
    ("vs", 0x28),
    ("ps", 0x30),
    ("mesh", 0x48),
    ("cs", 0x60),
];
/// Offset of the stage data from the record start.
const STAGE_DATA: usize = 0x54;
/// Most layout or used-resource entries accepted (a title run's blobs reach a few hundred; the
/// tables are bounds-checked against the payload anyway).
const MAX_TABLE: u32 = 512;
const MAX_IO: u32 = 32;
const PATCH_ENTRIES: usize = 8;
const PATCH_VARIANTS: usize = 10;
/// Dwords reserved per patch variant.
const PATCH_VARIANT_DWORDS: usize = 10;
/// Names of the patch variants, in table order.
pub const PATCH_VARIANT_NAMES: [&str; PATCH_VARIANTS] = [
    "ZERO", "32_R", "32_GR", "32_AR", "FP16", "UNORM16", "SNORM16", "UINT16", "SINT16", "32_ABGR",
];

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn short(what: &str, at: usize, have: usize) -> ShaderError {
    ShaderError::BadOffset(format!(
        "{what}: read at {at:#x} past the {have:#x}-byte payload"
    ))
}

/// One root-layout table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootLayout {
    /// 0 constants, 1 root descriptor, 3 table, 4 static sampler, 5/6 heaps (VIctory).
    pub root_type: u32,
    /// Table offset in dwords.
    pub table_offset: u32,
    pub user_sgprs: u32,
    pub base_register: u32,
    pub first_user_dword: u8,
    pub root_slot: u8,
}

/// One used-resource table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsedResource {
    /// 0 SRV, 1 sampler, 2 CBV, 3 UAV.
    pub kind: u8,
    pub root_type: u8,
    pub table_offset: u32,
    pub lower_bound: u32,
    pub stride: u32,
    pub descriptor_dwords: u8,
    pub first_user_dword: u8,
    pub root_slot: u8,
}

/// A vertex-shader input (`{usage, signature row, channel mask, first VGPR, last channel}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsInput {
    pub usage: u8,
    pub row: u8,
    pub mask: u8,
    pub first_vgpr: u8,
    pub last_channel: u8,
}

/// A vertex-shader output (`{usage, row, param n, row, slot, flags}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsOutput {
    pub usage: u8,
    pub row: u8,
    pub param: u8,
    pub row2: u8,
    pub slot: u8,
    pub flags: u8,
}

/// A pixel-shader input (`{usage, row, mask, attr, default, 2nd row, param slot, flags}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsInput {
    pub usage: u8,
    pub row: u8,
    pub mask: u8,
    pub attr: u8,
    pub default: u8,
    pub row2: u8,
    pub param_slot: u8,
    /// Bit 0 flat, 1 linear, 3 fp16, 4 packed fp16.
    pub flags: u8,
}

/// One export patch: where the code to replace sits, and the replacement per variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsPatch {
    /// Render target slot.
    pub target: usize,
    pub code_dword: u32,
    /// `PATCH_VARIANT_NAMES` order, each `patch_dwords` dwords.
    pub variants: Vec<Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VsData {
    pub inputs: Vec<VsInput>,
    pub outputs: Vec<VsOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsData {
    pub inputs: Vec<PsInput>,
    pub spi_ps_input_addr: u32,
    pub db_shader_control: u32,
    pub patch_dwords: u32,
    pub patches: Vec<PsPatch>,
}

/// A compute program's dispatch settings (`docs/shaders.md`, Inputs and outputs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsData {
    /// `COMPUTE_PGM_RSRC2`: scratch (bit 0), user SGPR count (5:1), thread-group id x/y/z
    /// (bits 7-9) and info (bit 10) SGPRs, thread-id VGPR count - 1 (12:11).
    pub rsrc2: u32,
    /// Thread-group size (`numthreads`).
    pub group: [u32; 3],
}

impl CsData {
    /// SGPRs the user data fills, from s0.
    pub fn user_sgprs(&self) -> u16 {
        (self.rsrc2 >> 1 & 0x1f) as u16
    }

    /// The SGPRs after the user data, in order: the enabled thread-group ids (x, y, z), the
    /// thread-group info, then the scratch wave offset.
    pub fn system_sgprs(&self) -> Vec<u16> {
        let mut next = self.user_sgprs();
        let mut out = Vec::new();
        for bit in [7, 8, 9, 10, 0] {
            if self.rsrc2 >> bit & 1 != 0 {
                out.push(next);
                next += 1;
            }
        }
        out
    }

    /// Thread-id VGPRs, v0.. (1 to 3).
    pub fn thread_id_vgprs(&self) -> u16 {
        (self.rsrc2 >> 11 & 3) as u16 + 1
    }
}

/// A mesh program's stage data: its outputs read as a vertex shader's (the parameter byte's
/// bit 7 marks a per-primitive output) and its group settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshData {
    pub outputs: Vec<VsOutput>,
    /// Thread-group size in lanes.
    pub group_lanes: u16,
    pub max_vertices: u16,
    pub max_primitives: u16,
    /// User SGPRs, from s8.
    pub user_sgprs: u8,
    /// Output topology (2 triangles).
    pub topology: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageData {
    Vertex(VsData),
    Pixel(PsData),
    Compute(CsData),
    Mesh(MeshData),
}

/// A decoded stage record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Payload offset of the record.
    pub at: usize,
    pub code_offset: u64,
    pub code_size: u64,
    pub flags: u32,
    pub lds_bytes: u32,
    pub vgprs: u32,
    pub sgprs: u32,
    pub layout: Vec<RootLayout>,
    pub used: Vec<UsedResource>,
    pub data: StageData,
}

impl Record {
    /// Wave32 (flags bit 5; clear = wave64) **[VIctory]**.
    pub fn wave32(&self) -> bool {
        self.flags & 0x20 != 0
    }
}

/// A native program: the `FSSM` payload with the slots it fills.
#[derive(Clone, Debug)]
pub struct NativeProgram {
    pub payload: Vec<u8>,
    /// Slots with a record, `(name, payload offset)`, in slot order.
    pub present: Vec<(&'static str, u64)>,
    pub vs: Option<Record>,
    pub ps: Option<Record>,
    pub cs: Option<Record>,
    pub mesh: Option<Record>,
}

impl NativeProgram {
    /// Parses an `XBCH` entry. Refuses an unknown version, an unpacked size over 16 MiB, a bad
    /// zlib stream, a size that disagrees with the header, and any table or code range outside
    /// the payload.
    pub fn parse(xbch: &[u8]) -> Result<Self, ShaderError> {
        if xbch.len() < XBCH_HEADER {
            return Err(ShaderError::Truncated(format!(
                "XBCH entry: {} bytes, need {XBCH_HEADER}",
                xbch.len()
            )));
        }
        if &xbch[0..4] != b"XBCH" {
            return Err(ShaderError::BadMagic("XBCH"));
        }
        let version = u32_at(xbch, 4).expect("header read");
        if version != 3 {
            return Err(ShaderError::Unsupported(format!("XBCH version {version}")));
        }
        let (packed, unpacked) = (
            u64_at(xbch, 8).expect("header read"),
            u64_at(xbch, 16).expect("header read"),
        );
        if unpacked > MAX_UNPACKED {
            return Err(ShaderError::TooLarge(format!(
                "XBCH unpacked size {unpacked} (limit {MAX_UNPACKED})"
            )));
        }
        let stream = &xbch[XBCH_HEADER..];
        if packed != stream.len() as u64 {
            return Err(ShaderError::BadOffset(format!(
                "XBCH packed size {packed}, entry holds {}",
                stream.len()
            )));
        }
        let data =
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(stream, unpacked as usize)
                .map_err(|e| ShaderError::BadZlib(format!("{:?}", e.status)))?;
        if data.len() as u64 != unpacked {
            return Err(ShaderError::BadZlib(format!(
                "inflated {} bytes, header says {unpacked}",
                data.len()
            )));
        }
        if data.len() < FSSM_HEADER {
            return Err(ShaderError::Truncated(format!(
                "FSSM data: {} bytes, need {FSSM_HEADER}",
                data.len()
            )));
        }
        if &data[0..4] != b"FSSM" {
            return Err(ShaderError::BadMagic("FSSM"));
        }
        let fssm = u32_at(&data, 4).expect("header read");
        if fssm != 0x101 {
            return Err(ShaderError::Unsupported(format!("FSSM version {fssm:#x}")));
        }
        let size = u64_at(&data, 8).expect("header read");
        if size != (data.len() - FSSM_HEADER) as u64 {
            return Err(ShaderError::BadOffset(format!(
                "FSSM payload size {size}, data holds {}",
                data.len() - FSSM_HEADER
            )));
        }
        Self::from_payload(data[FSSM_HEADER..].to_vec())
    }

    fn from_payload(payload: Vec<u8>) -> Result<Self, ShaderError> {
        // The slot table is 0x68 bytes: a shorter payload has none of it.
        if payload.len() < 0x68 {
            return Err(ShaderError::Truncated(format!(
                "FSSM payload: {} bytes, need 0x68 for the slot table",
                payload.len()
            )));
        }
        let kinds = u64_at(&payload, 0).expect("checked");
        if kinds != 6 {
            return Err(ShaderError::Unsupported(format!(
                "FSSM payload starts with {kinds}, expected 6"
            )));
        }
        let (mut vs, mut ps, mut cs, mut mesh) = (None, None, None, None);
        let mut present = Vec::new();
        for (name, slot) in SLOTS {
            let Some(o) = u64_at(&payload, slot).filter(|&o| o != ABSENT) else {
                continue;
            };
            present.push((name, o));
            match name {
                "vs" => vs = Some(decode_record(&payload, o, Stage::Vertex)?),
                "ps" => ps = Some(decode_record(&payload, o, Stage::Pixel)?),
                "cs" => cs = Some(decode_record(&payload, o, Stage::Compute)?),
                "mesh" => mesh = Some(decode_record(&payload, o, Stage::Mesh)?),
                _ => {}
            }
        }
        Ok(NativeProgram {
            payload,
            present,
            vs,
            ps,
            cs,
            mesh,
        })
    }

    /// The decoded records, `(slot name, record)`, in slot order.
    pub fn records(&self) -> Vec<(&'static str, &Record)> {
        [
            ("vs", &self.vs),
            ("ps", &self.ps),
            ("mesh", &self.mesh),
            ("cs", &self.cs),
        ]
        .into_iter()
        .filter_map(|(n, r)| r.as_ref().map(|r| (n, r)))
        .collect()
    }

    /// The code of every slot with a record, `(slot name, bytes)`; records of stages without a
    /// decoder (geometry, hull...) are read only as far as their code range.
    pub fn slot_codes(&self) -> Vec<(&'static str, &[u8])> {
        let p = &self.payload;
        self.present
            .iter()
            .filter_map(|&(name, at)| {
                let at = at as usize;
                let o = u64_at(p, at)? as usize;
                let n = u64_at(p, at + 8)? as usize;
                Some((name, p.get(o..o.checked_add(n)?)?))
            })
            .collect()
    }

    /// A record's code bytes.
    pub fn code(&self, r: &Record) -> &[u8] {
        let (o, n) = (r.code_offset as usize, r.code_size as usize);
        &self.payload[o..o + n]
    }
}

/// The payload slice `[at, at + len)`, or an error naming `what`.
fn range(p: &[u8], at: u64, len: u64, what: &str) -> Result<usize, ShaderError> {
    match at.checked_add(len) {
        Some(end) if end <= p.len() as u64 => Ok(at as usize),
        _ => Err(ShaderError::BadOffset(format!(
            "{what} at {at:#x}+{len:#x} past the {:#x}-byte payload",
            p.len()
        ))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Vertex,
    Pixel,
    Compute,
    Mesh,
}

fn decode_record(p: &[u8], at: u64, stage: Stage) -> Result<Record, ShaderError> {
    let r = range(p, at, STAGE_DATA as u64, "record")?;
    let w = |o: usize| u32_at(p, r + o).ok_or_else(|| short("record", r + o, p.len()));
    let q = |o: usize| u64_at(p, r + o).ok_or_else(|| short("record", r + o, p.len()));
    let (code_offset, code_size) = (q(0)?, q(8)?);
    range(p, code_offset, code_size, "code")?;
    let (layout_at, used_at) = (q(0x28)?, q(0x30)?);
    let (layout_n, used_n) = (w(0x38)?, w(0x3c)?);
    if layout_n > MAX_TABLE || used_n > MAX_TABLE {
        return Err(ShaderError::Unsupported(format!(
            "{layout_n} layout and {used_n} used-resource entries"
        )));
    }
    let table = |at: u64, n: u32, what: &str| -> Result<usize, ShaderError> {
        if n == 0 {
            Ok(0)
        } else {
            range(p, at, 20 * n as u64, what)
        }
    };
    let (lt, ut) = (
        table(layout_at, layout_n, "root layout table")?,
        table(used_at, used_n, "used-resource table")?,
    );
    let layout = (0..layout_n as usize)
        .map(|i| {
            let e = &p[lt + 20 * i..lt + 20 * i + 20];
            let d = |k: usize| u32_at(e, 4 * k).expect("20 bytes");
            RootLayout {
                root_type: d(0),
                table_offset: d(1),
                user_sgprs: d(2),
                base_register: d(3),
                first_user_dword: e[16],
                root_slot: e[17],
            }
        })
        .collect();
    let used = (0..used_n as usize)
        .map(|i| {
            let e = &p[ut + 20 * i..ut + 20 * i + 20];
            let d = |k: usize| u32_at(e, 4 * k).expect("20 bytes");
            UsedResource {
                kind: e[0],
                root_type: e[1],
                table_offset: d(1),
                lower_bound: d(2),
                stride: d(3),
                descriptor_dwords: e[16],
                first_user_dword: e[17],
                root_slot: e[18],
            }
        })
        .collect();
    let regs = w(0x50)?;
    let e = r + STAGE_DATA;
    let data = match stage {
        Stage::Vertex => StageData::Vertex(decode_vs(p, e)?),
        Stage::Pixel => StageData::Pixel(decode_ps(p, e)?),
        Stage::Compute => StageData::Compute(decode_cs(p, e)?),
        Stage::Mesh => StageData::Mesh(decode_mesh(p, e)?),
    };
    Ok(Record {
        at: r,
        code_offset,
        code_size,
        flags: w(0x14)?,
        lds_bytes: w(0x4c)?,
        vgprs: regs & 0x3ff,
        sgprs: (regs >> 20) & 0xff,
        layout,
        used,
        data,
    })
}

fn count(p: &[u8], at: usize, what: &str) -> Result<usize, ShaderError> {
    let n = u32_at(p, at).ok_or_else(|| short(what, at, p.len()))?;
    if n > MAX_IO {
        return Err(ShaderError::Unsupported(format!("{what}: {n} entries")));
    }
    Ok(n as usize)
}

fn decode_vs(p: &[u8], e: usize) -> Result<VsData, ShaderError> {
    let n_in = count(p, e + 0xc4, "VS inputs")?;
    let inputs = (0..n_in)
        .map(|i| {
            let b = p.get(e + 0x24 + 5 * i..e + 0x24 + 5 * i + 5)?;
            Some(VsInput {
                usage: b[0],
                row: b[1],
                mask: b[2],
                first_vgpr: b[3],
                last_channel: b[4],
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| short("VS inputs", e + 0x24, p.len()))?;
    let outputs = decode_outputs(p, e, "VS outputs")?;
    Ok(VsData { inputs, outputs })
}

/// The 6-byte output entries at E+0xc8 (count at E+0x188), shared by vertex and mesh programs.
fn decode_outputs(p: &[u8], e: usize, what: &str) -> Result<Vec<VsOutput>, ShaderError> {
    let n = count(p, e + 0x188, what)?;
    (0..n)
        .map(|i| {
            let b = p.get(e + 0xc8 + 6 * i..e + 0xc8 + 6 * i + 6)?;
            Some(VsOutput {
                usage: b[0],
                row: b[1],
                param: b[2],
                row2: b[3],
                slot: b[4],
                flags: b[5],
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| short(what, e + 0xc8, p.len()))
}

/// Most threads in a compute group (Direct3D 12).
const MAX_GROUP_THREADS: u64 = 1024;

fn decode_cs(p: &[u8], e: usize) -> Result<CsData, ShaderError> {
    let w = |o: usize| u32_at(p, e + o).ok_or_else(|| short("CS data", e + o, p.len()));
    let group = [w(0x28)?, w(0x2c)?, w(0x30)?];
    let threads: u64 = group.iter().map(|&g| u64::from(g)).product();
    if group.contains(&0) || threads > MAX_GROUP_THREADS {
        return Err(ShaderError::Unsupported(format!(
            "thread group {}x{}x{} (1 to {MAX_GROUP_THREADS} threads)",
            group[0], group[1], group[2]
        )));
    }
    Ok(CsData {
        rsrc2: w(0x24)?,
        group,
    })
}

fn decode_mesh(p: &[u8], e: usize) -> Result<MeshData, ShaderError> {
    let outputs = decode_outputs(p, e, "mesh outputs")?;
    let info = p
        .get(e + 0x448..e + 0x450)
        .ok_or_else(|| short("mesh data", e + 0x448, p.len()))?;
    let half = |at: usize| u16::from_le_bytes([info[at], info[at + 1]]);
    let m = MeshData {
        outputs,
        group_lanes: half(0),
        max_vertices: half(2),
        max_primitives: half(4),
        user_sgprs: info[6],
        topology: info[7],
    };
    if m.group_lanes == 0 || u64::from(m.group_lanes) > MAX_GROUP_THREADS {
        return Err(ShaderError::Unsupported(format!(
            "mesh group of {} lanes (1 to {MAX_GROUP_THREADS})",
            m.group_lanes
        )));
    }
    Ok(m)
}

fn decode_ps(p: &[u8], e: usize) -> Result<PsData, ShaderError> {
    let n_in = count(p, e + 0x124, "PS inputs")?;
    let inputs = (0..n_in)
        .map(|i| {
            let b = p.get(e + 0x24 + 8 * i..e + 0x24 + 8 * i + 8)?;
            Some(PsInput {
                usage: b[0],
                row: b[1],
                mask: b[2],
                attr: b[3],
                default: b[4],
                row2: b[5],
                param_slot: b[6],
                flags: b[7],
            })
        })
        .collect::<Option<_>>()
        .ok_or_else(|| short("PS inputs", e + 0x24, p.len()))?;
    let w = |o: usize| u32_at(p, e + o).ok_or_else(|| short("PS data", e + o, p.len()));
    let patch_dwords = w(0x148)?;
    if patch_dwords as usize > PATCH_VARIANT_DWORDS {
        return Err(ShaderError::Unsupported(format!(
            "export patch of {patch_dwords} dwords (at most {PATCH_VARIANT_DWORDS})"
        )));
    }
    let entry = 4 + 4 * PATCH_VARIANTS * PATCH_VARIANT_DWORDS;
    let table = e + 0x14c;
    if table + PATCH_ENTRIES * entry > p.len() {
        return Err(short("PS export patch table", table, p.len()));
    }
    let mut patches = Vec::new();
    for t in 0..PATCH_ENTRIES {
        let at = table + t * entry;
        let off = u32_at(p, at).expect("bounds checked") as i32;
        if off < 0 {
            continue;
        }
        let variants = (0..PATCH_VARIANTS)
            .map(|v| {
                let base = at + 4 + 4 * PATCH_VARIANT_DWORDS * v;
                (0..patch_dwords as usize)
                    .map(|k| u32_at(p, base + 4 * k).expect("bounds checked"))
                    .collect()
            })
            .collect();
        patches.push(PsPatch {
            target: t,
            code_dword: off as u32,
            variants,
        });
    }
    Ok(PsData {
        inputs,
        spi_ps_input_addr: w(0x134)?,
        db_shader_control: w(0x138)?,
        patch_dwords,
        patches,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::shader::container::tests::put32;

    /// A synthetic record at `at` in `p`: code of 8 bytes at 0x200, one layout entry, no used.
    fn put_u64(p: &mut [u8], at: usize, v: u64) {
        p[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u32(p: &mut [u8], at: usize, v: u32) {
        p[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u16(p: &mut [u8], at: usize, v: u16) {
        p[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// A payload with a VS record (`vs`) and/or a PS record (`ps`), 0x2000 bytes.
    pub fn payload(vs: bool, ps: bool) -> Vec<u8> {
        payload_all(vs, ps, false, false)
    }

    /// Records at 0x300 (VS), 0x700 (PS), 0x1500 (CS) and 0x1600 (mesh); the PS patch table
    /// runs to 0x1440, so every combination fits.
    pub fn payload_all(vs: bool, ps: bool, cs: bool, mesh: bool) -> Vec<u8> {
        let mut p = vec![0u8; 0x2000];
        put_u64(&mut p, 0, 6);
        for (_, o) in SLOTS {
            put_u64(&mut p, o, ABSENT);
        }
        // Code (8 bytes) at 0x200; layout table at 0x210; used table at 0x230.
        p[0x200..0x208].copy_from_slice(&[0xaa; 8]);
        put_u32(&mut p, 0x210, 1); // root type
        put_u32(&mut p, 0x218, 2); // user sgprs
        p[0x220] = 8; // first user dword
        p[0x221] = 3; // root slot
        p[0x230] = 2; // CBV
        p[0x231] = 1;
        put_u32(&mut p, 0x238, 5); // lower bound
        p[0x240] = 4;
        let all = [
            (vs, 0x28, 0x300usize),
            (ps, 0x30, 0x700),
            (cs, 0x60, 0x1500),
            (mesh, 0x48, 0x1600),
        ];
        for (on, slot, at) in all {
            if !on {
                continue;
            }
            put_u64(&mut p, slot, at as u64);
            put_u64(&mut p, at, 0x200);
            put_u64(&mut p, at + 8, 8);
            put_u32(&mut p, at + 0x14, 0xa0000);
            put_u64(&mut p, at + 0x28, 0x210);
            put_u64(&mut p, at + 0x30, 0x230);
            put_u32(&mut p, at + 0x38, 1);
            put_u32(&mut p, at + 0x3c, 1);
            put_u32(&mut p, at + 0x4c, 64);
            put_u32(&mut p, at + 0x50, 0xc00008);
            let e = at + STAGE_DATA;
            if slot == 0x60 {
                // scratch, 6 user SGPRs, group id x and y, group info, 2 thread-id VGPRs
                put_u32(
                    &mut p,
                    e + 0x24,
                    1 | 6 << 1 | 0x80 | 0x100 | 0x400 | 1 << 11,
                );
                for (k, g) in [8u32, 4, 2].into_iter().enumerate() {
                    put_u32(&mut p, e + 0x28 + 4 * k, g);
                }
            } else if slot == 0x48 {
                p[e + 0xc8..e + 0xce].copy_from_slice(&[7, 2, 0x80, 2, 1, 3]);
                put_u32(&mut p, e + 0x188, 1);
                p[e + 0x448..e + 0x450].copy_from_slice(&[128, 0, 64, 0, 126, 0, 5, 2]);
            } else if slot == 0x28 {
                p[e + 0x24..e + 0x29].copy_from_slice(&[1, 2, 3, 4, 5]);
                put_u32(&mut p, e + 0xc4, 1);
                p[e + 0xc8..e + 0xce].copy_from_slice(&[6, 1, 0, 0, 0, 9]);
                put_u32(&mut p, e + 0x188, 1);
            } else {
                p[e + 0x24..e + 0x2c].copy_from_slice(&[6, 1, 0xf, 0, 0, 0, 1, 1]);
                put_u32(&mut p, e + 0x124, 1);
                put_u32(&mut p, e + 0x134, 2);
                put_u32(&mut p, e + 0x138, 0x10);
                put_u32(&mut p, e + 0x148, 6);
                for t in 0..PATCH_ENTRIES {
                    let o = e + 0x14c + t * 404;
                    put_u32(&mut p, o, if t == 0 { 9 } else { u32::MAX });
                }
                // Variant 9 of target 0: six distinctive dwords.
                for k in 0..6 {
                    put_u32(&mut p, e + 0x14c + 4 + 40 * 9 + 4 * k, 0x100 + k as u32);
                }
            }
        }
        p
    }

    /// An `XBCH` entry around `payload`.
    pub fn xbch(payload: &[u8]) -> Vec<u8> {
        let mut data = b"FSSM".to_vec();
        put32(&mut data, 0x101);
        data.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        data.extend_from_slice(payload);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&data, 6);
        let mut v = b"XBCH".to_vec();
        put32(&mut v, 3);
        v.extend_from_slice(&(z.len() as u64).to_le_bytes());
        v.extend_from_slice(&(data.len() as u64).to_le_bytes());
        v.extend(z);
        v
    }

    #[test]
    fn parses_vertex_and_pixel_records() {
        let n = NativeProgram::parse(&xbch(&payload(true, true))).unwrap();
        assert_eq!(n.present, [("vs", 0x300), ("ps", 0x700)]);
        let vs = n.vs.as_ref().unwrap();
        assert_eq!((vs.code_offset, vs.code_size), (0x200, 8));
        assert_eq!(n.code(vs), &[0xaa; 8]);
        assert!(!vs.wave32());
        assert_eq!((vs.lds_bytes, vs.vgprs, vs.sgprs), (64, 8, 0xc));
        assert_eq!(
            vs.layout,
            [RootLayout {
                root_type: 1,
                table_offset: 0,
                user_sgprs: 2,
                base_register: 0,
                first_user_dword: 8,
                root_slot: 3
            }]
        );
        assert_eq!(vs.used[0].kind, 2);
        assert_eq!(
            (vs.used[0].lower_bound, vs.used[0].descriptor_dwords),
            (5, 4)
        );
        let StageData::Vertex(d) = &vs.data else {
            panic!("not a VS")
        };
        assert_eq!(
            d.inputs,
            [VsInput {
                usage: 1,
                row: 2,
                mask: 3,
                first_vgpr: 4,
                last_channel: 5
            }]
        );
        assert_eq!((d.outputs[0].usage, d.outputs[0].flags), (6, 9));
        let ps = n.ps.as_ref().unwrap();
        let StageData::Pixel(d) = &ps.data else {
            panic!("not a PS")
        };
        assert_eq!(d.inputs[0].mask, 0xf);
        assert_eq!(d.inputs[0].flags, 1);
        assert_eq!((d.spi_ps_input_addr, d.db_shader_control), (2, 0x10));
        assert_eq!(d.patches.len(), 1);
        assert_eq!(d.patches[0].code_dword, 9);
        assert_eq!(d.patches[0].variants.len(), 10);
        assert_eq!(
            d.patches[0].variants[9],
            [0x100, 0x101, 0x102, 0x103, 0x104, 0x105]
        );
    }

    #[test]
    fn absent_stages_and_other_slots() {
        let mut p = payload(false, true);
        put_u64(&mut p, 0x20, 0x400); // a geometry slot: reported, not decoded
        let n = NativeProgram::parse(&xbch(&p)).unwrap();
        assert!(n.vs.is_none() && n.cs.is_none() && n.mesh.is_none());
        assert_eq!(n.present, [("gs", 0x400), ("ps", 0x700)]);
    }

    #[test]
    fn parses_compute_and_mesh_records() {
        let n = NativeProgram::parse(&xbch(&payload_all(false, false, true, true))).unwrap();
        assert_eq!(n.present, [("mesh", 0x1600), ("cs", 0x1500)]);
        assert_eq!(
            n.records().iter().map(|r| r.0).collect::<Vec<_>>(),
            ["mesh", "cs"]
        );
        let StageData::Compute(c) = &n.cs.as_ref().unwrap().data else {
            panic!("not a CS")
        };
        assert_eq!(c.group, [8, 4, 2]);
        assert_eq!(c.user_sgprs(), 6);
        // x and y ids at s6 and s7, info at s8, scratch offset at s9.
        assert_eq!(c.system_sgprs(), [6, 7, 8, 9]);
        assert_eq!(c.thread_id_vgprs(), 2);
        let plain = CsData {
            rsrc2: 0,
            group: [1; 3],
        };
        assert_eq!(plain.system_sgprs(), Vec::<u16>::new());
        assert_eq!((plain.user_sgprs(), plain.thread_id_vgprs()), (0, 1));
        let m = n.mesh.as_ref().unwrap();
        assert_eq!((m.code_offset, m.code_size), (0x200, 8));
        let StageData::Mesh(d) = &m.data else {
            panic!("not a mesh")
        };
        assert_eq!(
            (d.group_lanes, d.max_vertices, d.max_primitives),
            (128, 64, 126)
        );
        assert_eq!((d.user_sgprs, d.topology), (5, 2));
        assert_eq!(d.outputs.len(), 1);
        assert_eq!((d.outputs[0].usage, d.outputs[0].param), (7, 0x80));
    }

    #[test]
    fn empty_tables_ignore_their_offset() {
        let mut p = payload(true, false);
        put_u32(&mut p, 0x300 + 0x38, 0);
        put_u32(&mut p, 0x300 + 0x3c, 0);
        put_u64(&mut p, 0x300 + 0x30, ABSENT);
        let n = NativeProgram::parse(&xbch(&p)).unwrap();
        assert!(n.vs.unwrap().used.is_empty());
    }

    #[test]
    fn refuses_bad_xbch() {
        let good = xbch(&payload(true, false));
        assert!(matches!(
            NativeProgram::parse(&good[..10]),
            Err(ShaderError::Truncated(_))
        ));
        let mut b = good.clone();
        b[0] = b'Y';
        assert!(matches!(
            NativeProgram::parse(&b),
            Err(ShaderError::BadMagic("XBCH"))
        ));
        let mut b = good.clone();
        b[4] = 4;
        assert!(matches!(
            NativeProgram::parse(&b),
            Err(ShaderError::Unsupported(_))
        ));
        let mut b = good.clone();
        b[16..24].copy_from_slice(&(MAX_UNPACKED + 1).to_le_bytes());
        assert!(matches!(
            NativeProgram::parse(&b),
            Err(ShaderError::TooLarge(_))
        ));
        let mut b = good.clone();
        b[8] ^= 1; // packed size
        assert!(matches!(
            NativeProgram::parse(&b),
            Err(ShaderError::BadOffset(_))
        ));
        // Garbage stream with a consistent header.
        let mut b = good[..24].to_vec();
        b.extend_from_slice(&[0x55; 20]);
        b[8..16].copy_from_slice(&20u64.to_le_bytes());
        assert!(matches!(
            NativeProgram::parse(&b),
            Err(ShaderError::BadZlib(_))
        ));
        // Unpacked size smaller or larger than what the stream holds.
        let n = u64::from_le_bytes(good[16..24].try_into().unwrap());
        for wrong in [n - 1, n + 1] {
            let mut b = good.clone();
            b[16..24].copy_from_slice(&wrong.to_le_bytes());
            assert!(matches!(
                NativeProgram::parse(&b),
                Err(ShaderError::BadZlib(_))
            ));
        }
    }

    /// An `XBCH` entry around raw (already framed) unpacked bytes.
    fn xbch_raw(data: &[u8]) -> Vec<u8> {
        let z = miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
        let mut v = b"XBCH".to_vec();
        put32(&mut v, 3);
        v.extend_from_slice(&(z.len() as u64).to_le_bytes());
        v.extend_from_slice(&(data.len() as u64).to_le_bytes());
        v.extend(z);
        v
    }

    #[test]
    fn refuses_bad_fssm() {
        let p = payload(true, false);
        let frame = |magic: &[u8; 4], version: u32, size: u64, body: &[u8]| {
            let mut d = magic.to_vec();
            put32(&mut d, version);
            d.extend_from_slice(&size.to_le_bytes());
            d.extend_from_slice(body);
            xbch_raw(&d)
        };
        let n = p.len() as u64;
        assert!(matches!(
            NativeProgram::parse(&xbch_raw(&[1, 2, 3])),
            Err(ShaderError::Truncated(_))
        ));
        assert!(matches!(
            NativeProgram::parse(&frame(b"FSSX", 0x101, n, &p)),
            Err(ShaderError::BadMagic("FSSM"))
        ));
        assert!(matches!(
            NativeProgram::parse(&frame(b"FSSM", 0x102, n, &p)),
            Err(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            NativeProgram::parse(&frame(b"FSSM", 0x101, n + 1, &p)),
            Err(ShaderError::BadOffset(_))
        ));
        assert!(matches!(
            NativeProgram::parse(&frame(b"FSSM", 0x101, 8, &p[..8])),
            Err(ShaderError::Truncated(_))
        ));
        let mut q = p.clone();
        q[0] = 7; // slot count
        assert!(matches!(
            NativeProgram::parse(&xbch(&q)),
            Err(ShaderError::Unsupported(_))
        ));
    }

    #[test]
    fn refuses_bad_records() {
        let bad = |edit: &dyn Fn(&mut Vec<u8>), ps: bool| {
            let mut p = payload(!ps, ps);
            edit(&mut p);
            NativeProgram::parse(&xbch(&p)).err()
        };
        let (vs_at, ps_at) = (0x300, 0x700);
        // Record offset past the payload.
        assert!(matches!(
            bad(&|p| put_u64(p, 0x28, 0x1ff0), false),
            Some(ShaderError::BadOffset(_))
        ));
        // Code range past the payload (offset, then size).
        assert!(matches!(
            bad(&|p| put_u64(p, vs_at, 0x1fff), false),
            Some(ShaderError::BadOffset(_))
        ));
        assert!(matches!(
            bad(&|p| put_u64(p, vs_at + 8, u64::MAX), false),
            Some(ShaderError::BadOffset(_))
        ));
        // Tables: too many, and past the payload.
        assert!(matches!(
            bad(&|p| put_u32(p, vs_at + 0x38, 513), false),
            Some(ShaderError::Unsupported(_))
        ));
        // Real programs list a few hundred entries (the cap was 64 and refused 2486 blobs).
        assert!(bad(&|p| put_u32(p, vs_at + 0x38, 300), false).is_none());
        assert!(matches!(
            bad(&|p| put_u64(p, vs_at + 0x28, 0x1ff8), false),
            Some(ShaderError::BadOffset(_))
        ));
        assert!(matches!(
            bad(&|p| put_u64(p, vs_at + 0x30, 0x1ff8), false),
            Some(ShaderError::BadOffset(_))
        ));
        // Stage data counts.
        assert!(matches!(
            bad(&|p| put_u32(p, vs_at + STAGE_DATA + 0xc4, 33), false),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad(&|p| put_u32(p, ps_at + STAGE_DATA + 0x124, 33), true),
            Some(ShaderError::Unsupported(_))
        ));
        // Compute: a group of no thread, of too many, and stage data past the payload.
        let (cs_at, ms_at) = (0x1500, 0x1600);
        let bad_all = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut p = payload_all(false, false, true, true);
            edit(&mut p);
            NativeProgram::parse(&xbch(&p)).err()
        };
        assert!(bad_all(&|_| {}).is_none());
        assert!(matches!(
            bad_all(&|p| put_u32(p, cs_at + STAGE_DATA + 0x2c, 0)),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad_all(&|p| put_u32(p, cs_at + STAGE_DATA + 0x28, 1025)),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad_all(&|p| {
                // A record whose stage data ends inside the payload but whose group size
                // dwords do not: the record moves to the very end.
                put_u64(p, 0x60, 0x2000 - STAGE_DATA as u64 - 8);
                let at = 0x2000 - STAGE_DATA - 8;
                p.copy_within(cs_at..cs_at + STAGE_DATA, at);
            }),
            Some(ShaderError::BadOffset(_))
        ));
        // Mesh: too many outputs, no lanes, too many lanes, info past the payload.
        assert!(matches!(
            bad_all(&|p| put_u32(p, ms_at + STAGE_DATA + 0x188, 33)),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad_all(&|p| put_u16(p, ms_at + STAGE_DATA + 0x448, 0)),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad_all(&|p| put_u16(p, ms_at + STAGE_DATA + 0x448, 1025)),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad_all(&|p| {
                put_u64(p, 0x48, 0x1c00);
                p.copy_within(ms_at..ms_at + STAGE_DATA, 0x1c00);
            }),
            Some(ShaderError::BadOffset(_))
        ));
        // Patch size, and a patch table running past the payload.
        assert!(matches!(
            bad(&|p| put_u32(p, ps_at + STAGE_DATA + 0x148, 11), true),
            Some(ShaderError::Unsupported(_))
        ));
        assert!(matches!(
            bad(&|p| put_u64(p, 0x30, 0x1400), true),
            Some(ShaderError::BadOffset(_))
        ));
    }
}
