//! The shader translator's front end (milestone G4, `docs/shaders.md`): reading a shader blob
//! the way the title hands it to `CreateGraphicsPipelineState`: a DXBC container whose `DXIL`
//! part carries the console's native program (GFX10 machine code plus stage data).
//!
//! Chunk 1 only parses and dumps: `container` is the DXBC layer, `native` the program inside.
//! `NATIV64_DUMP_SHADERS=<dir>` writes every distinct blob a pipeline is created with, with a
//! text summary; `nativ64-shader <blob>` prints the same summary for a file.

pub mod container;
pub mod gfx10;
pub mod native;

use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::sync::Mutex;

use container::{Container, Dxil, ProgramKind, Signature, XDAT_NATIVE};
use native::{NativeProgram, Record, StageData, PATCH_VARIANT_NAMES};

/// Why a blob was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderError {
    /// Fewer bytes than a header or table needs.
    Truncated(String),
    BadMagic(&'static str),
    /// A version, count or layout we do not read.
    Unsupported(String),
    /// An offset or size outside what holds it.
    BadOffset(String),
    BadZlib(String),
    TooLarge(String),
    /// A required part is absent.
    Missing(&'static str),
}

impl fmt::Display for ShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(s) => write!(f, "truncated: {s}"),
            Self::BadMagic(m) => write!(f, "bad magic, expected {m}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::BadOffset(s) => write!(f, "bad offset: {s}"),
            Self::BadZlib(s) => write!(f, "bad zlib stream: {s}"),
            Self::TooLarge(s) => write!(f, "too large: {s}"),
            Self::Missing(s) => write!(f, "missing {s}"),
        }
    }
}

impl std::error::Error for ShaderError {}

/// A parsed shader blob.
#[derive(Debug)]
pub struct ParsedShader {
    pub kind: ProgramKind,
    pub program_version: (u8, u8),
    pub dxil_version: u32,
    pub parts: Vec<([u8; 4], usize)>,
    pub input_signature: Option<Signature>,
    pub output_signature: Option<Signature>,
    /// The native program holding a record for this blob's stage.
    pub native: NativeProgram,
    /// Number of native programs the blob carries.
    pub native_programs: usize,
}

impl ParsedShader {
    /// The record of the blob's own stage (vertex or pixel).
    pub fn record(&self) -> Option<&Record> {
        match self.kind {
            ProgramKind::Vertex => self.native.vs.as_ref(),
            ProgramKind::Pixel => self.native.ps.as_ref(),
            _ => None,
        }
    }
}

/// Parses a blob: container, signatures, `DXIL` header, native program. A vertex blob may carry
/// two native programs (with and without a geometry stage): the first with a record for the
/// blob's stage is taken. Stages other than vertex and pixel parse as far as the container and
/// list their slots, with no record.
pub fn parse(bytes: &[u8]) -> Result<ParsedShader, ShaderError> {
    let c = Container::parse(bytes)?;
    let dxil: Dxil = c.dxil()?;
    let mut programs = Vec::new();
    for e in dxil.xdat.iter().filter(|e| e.kind == XDAT_NATIVE) {
        programs.push(NativeProgram::parse(dxil.entry(e))?);
    }
    let native_programs = programs.len();
    let wanted = |n: &NativeProgram| match dxil.kind {
        ProgramKind::Vertex => n.vs.is_some(),
        ProgramKind::Pixel => n.ps.is_some(),
        _ => true,
    };
    let native = programs
        .into_iter()
        .find(wanted)
        .ok_or(ShaderError::Missing(
            "native program with a record for the stage",
        ))?;
    Ok(ParsedShader {
        kind: dxil.kind,
        program_version: (dxil.major, dxil.minor),
        dxil_version: dxil.dxil_version,
        parts: c.parts.iter().map(|p| (p.fourcc, p.size)).collect(),
        input_signature: c.signature(b"ISG1")?,
        output_signature: c.signature(b"OSG1")?,
        native,
        native_programs,
    })
}

/// 64-bit FNV-1a, the hash of a blob in dump file names (and of cache keys, later).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn signature_text(out: &mut String, title: &str, s: &Option<Signature>) {
    let Some(s) = s else { return };
    let _ = writeln!(out, "{title} ({} elements)", s.elements.len());
    for e in &s.elements {
        let _ = writeln!(
            out,
            "  {}{} register {} mask {:#x} rw {:#x} sysval {} type {} stream {} prec {}",
            e.name,
            e.index,
            e.register,
            e.mask,
            e.rw_mask,
            e.system_value,
            e.component_type,
            e.stream,
            e.min_precision
        );
    }
}

impl fmt::Display for ParsedShader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

impl ParsedShader {
    /// Readable text: parts, DXIL header, signatures, then the record of the blob's stage.
    pub fn summary(&self) -> String {
        let mut o = String::new();
        let parts: Vec<String> = self
            .parts
            .iter()
            .map(|(f, n)| format!("{}:{n}", container::fourcc_str(f)))
            .collect();
        let _ = writeln!(o, "parts {}", parts.join(" "));
        let _ = writeln!(
            o,
            "stage {} program {}.{} dxil {:#x} native programs {}",
            self.kind.name(),
            self.program_version.0,
            self.program_version.1,
            self.dxil_version,
            self.native_programs
        );
        signature_text(&mut o, "input signature", &self.input_signature);
        signature_text(&mut o, "output signature", &self.output_signature);
        let slots: Vec<String> = self
            .native
            .present
            .iter()
            .map(|(n, at)| format!("{n}@{at:#x}"))
            .collect();
        let _ = writeln!(o, "slots {}", slots.join(" "));
        let Some(r) = self.record() else {
            let _ = writeln!(o, "no record decoded for this stage");
            return o;
        };
        let _ = writeln!(
            o,
            "code {:#x}+{:#x} wave{} flags {:#x} lds {} vgprs {} sgprs {:#x}",
            r.code_offset,
            r.code_size,
            if r.wave32() { 32 } else { 64 },
            r.flags,
            r.lds_bytes,
            r.vgprs,
            r.sgprs
        );
        for l in &r.layout {
            let _ = writeln!(
                o,
                "layout root type {} table dword {} user sgprs {} base reg {} first dword {} slot {}",
                l.root_type, l.table_offset, l.user_sgprs, l.base_register, l.first_user_dword,
                l.root_slot
            );
        }
        for u in &r.used {
            let kind = ["srv", "sampler", "cbv", "uav"]
                .get(usize::from(u.kind))
                .copied()
                .unwrap_or("?");
            let _ = writeln!(
                o,
                "used {kind} root type {} table dword {} lower {} stride {} dwords {} first dword {} slot {}",
                u.root_type, u.table_offset, u.lower_bound, u.stride, u.descriptor_dwords,
                u.first_user_dword, u.root_slot
            );
        }
        match &r.data {
            StageData::Vertex(d) => {
                for i in &d.inputs {
                    let _ = writeln!(
                        o,
                        "vs input usage {} row {} mask {:#x} first vgpr {} last channel {}",
                        i.usage, i.row, i.mask, i.first_vgpr, i.last_channel
                    );
                }
                for x in &d.outputs {
                    let _ = writeln!(
                        o,
                        "vs output usage {} row {} param {} row2 {} slot {} flags {:#x}",
                        x.usage, x.row, x.param, x.row2, x.slot, x.flags
                    );
                }
            }
            StageData::Pixel(d) => {
                for i in &d.inputs {
                    let _ = writeln!(
                        o,
                        "ps input usage {} row {} mask {:#x} attr {} default {} row2 {} param slot {} flags {:#x}",
                        i.usage, i.row, i.mask, i.attr, i.default, i.row2, i.param_slot, i.flags
                    );
                }
                let _ = writeln!(
                    o,
                    "ps SPI_PS_INPUT_ADDR {:#x} DB_SHADER_CONTROL {:#x} patch dwords {}",
                    d.spi_ps_input_addr, d.db_shader_control, d.patch_dwords
                );
                for p in &d.patches {
                    let _ = writeln!(
                        o,
                        "patch target {} at code dword {}",
                        p.target, p.code_dword
                    );
                    for (name, v) in PATCH_VARIANT_NAMES.iter().zip(&p.variants) {
                        let words: Vec<String> = v.iter().map(|w| format!("{w:08x}")).collect();
                        let _ = writeln!(o, "  {name}: {}", words.join(" "));
                    }
                }
            }
        }
        o
    }
}

/// Blobs already seen this run (parsed once, logged once, dumped once).
static SEEN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// Records `hash` as seen; true the first time.
fn first_time(hash: u64) -> bool {
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(hash)
}

/// Called with each stage of a pipeline at creation: parses a blob not seen before, logs a
/// parse failure once, and writes it to `$NATIV64_DUMP_SHADERS` when set. Never fails.
pub fn observe(stage: &str, bytes: &[u8]) {
    let hash = fnv1a64(bytes);
    if !first_time(hash) {
        return;
    }
    let parsed = parse(bytes);
    if let Err(e) = &parsed {
        eprintln!(
            "nativ64: shader {hash:016x} ({stage}, {} bytes): {e}",
            bytes.len()
        );
    }
    if let Some(dir) = std::env::var_os("NATIV64_DUMP_SHADERS") {
        if let Err(e) = dump(Path::new(&dir), stage, hash, bytes, &parsed) {
            eprintln!("nativ64: NATIV64_DUMP_SHADERS: {e}");
        }
    }
}

/// Writes `<hash>.<stage>.dxbc` and `<hash>.<stage>.txt` (the summary, or the parse error).
pub fn dump(
    dir: &Path,
    stage: &str,
    hash: u64,
    bytes: &[u8],
    parsed: &Result<ParsedShader, ShaderError>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{hash:016x}.{stage}.dxbc")), bytes)?;
    let text = match parsed {
        Ok(p) => p.summary(),
        Err(e) => format!("parse error: {e}\n"),
    };
    std::fs::write(dir.join(format!("{hash:016x}.{stage}.txt")), text)
}

/// Appends one line to `pipelines.txt` in the dump directory when it is set: the stage hashes
/// of a created pipeline, in creation order.
pub fn note_pipeline(stages: &[(&str, &[u8])], targets: &[u32]) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("NATIV64_DUMP_SHADERS") else {
        return;
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stages: Vec<String> = stages
        .iter()
        .map(|(s, b)| format!("{s}={:016x}", fnv1a64(b)))
        .collect();
    let line = format!("pipeline {n} {} targets {targets:?}\n", stages.join(" "));
    let _ = std::fs::create_dir_all(&dir);
    let file = Path::new(&dir).join("pipelines.txt");
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut f| f.write_all(line.as_bytes()))
    {
        eprintln!("nativ64: NATIV64_DUMP_SHADERS: pipelines.txt: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::container::tests::{container, dxil, sig};
    use super::native::tests::{payload, xbch};
    use super::*;

    fn blob(kind: u16, programs: &[Vec<u8>]) -> Vec<u8> {
        let entries: Vec<(u32, Vec<u8>)> = std::iter::once((0x12, vec![0; 16]))
            .chain(programs.iter().map(|p| (XDAT_NATIVE, p.clone())))
            .chain(std::iter::once((0x07, vec![0; 28])))
            .collect();
        container(&[
            (b"SFI0", vec![0; 8]),
            (b"ISG1", sig(&[("TEXCOORD", 0, 0, 1, 0xf)])),
            (b"OSG1", sig(&[("SV_Position", 0, 1, 0, 0xf)])),
            (b"PSV0", vec![0; 20]),
            (b"DXIL", dxil(kind, &entries)),
            (b"PRIV", vec![0; 12]),
        ])
    }

    #[test]
    fn vertex_blob_parses() {
        let p = parse(&blob(1, &[xbch(&payload(true, false))])).unwrap();
        assert_eq!(p.kind, ProgramKind::Vertex);
        assert_eq!(p.parts.len(), 6);
        assert_eq!(
            p.input_signature.as_ref().unwrap().elements[0].name,
            "TEXCOORD"
        );
        assert_eq!(
            p.output_signature.as_ref().unwrap().elements[0].name,
            "SV_Position"
        );
        assert!(matches!(p.record().unwrap().data, StageData::Vertex(_)));
        let s = p.summary();
        assert!(s.contains("stage vertex program 6.6"), "{s}");
        assert!(s.contains("vs output usage 6"), "{s}");
    }

    #[test]
    fn pixel_blob_parses_and_summarizes_the_patch() {
        let p = parse(&blob(0, &[xbch(&payload(false, true))])).unwrap();
        assert_eq!(p.kind, ProgramKind::Pixel);
        let s = p.summary();
        assert!(s.contains("patch target 0 at code dword 9"), "{s}");
        assert!(s.contains("32_ABGR: 00000100 00000101"), "{s}");
        assert!(s.contains("wave64"), "{s}");
    }

    #[test]
    fn second_native_program_is_chosen_when_the_first_has_no_record() {
        // A VS blob with a GS-style program (PS record only) then the real one.
        let two = [xbch(&payload(false, true)), xbch(&payload(true, false))];
        let p = parse(&blob(1, &two)).unwrap();
        assert_eq!(p.native_programs, 2);
        assert!(p.native.vs.is_some());
    }

    #[test]
    fn other_stage_kinds_parse_without_a_record() {
        let mut pl = payload(false, false);
        pl[0x60..0x68].copy_from_slice(&0x400u64.to_le_bytes());
        let p = parse(&blob(5, &[xbch(&pl)])).unwrap();
        assert_eq!(p.kind, ProgramKind::Compute);
        assert!(p.record().is_none());
        assert!(p.summary().contains("no record decoded"));
    }

    #[test]
    fn refusals_surface() {
        assert_eq!(
            parse(&[]).unwrap_err(),
            ShaderError::Truncated("container header: 0 bytes, need 32".into())
        );
        // No native program at all (a plain DXIL part).
        let plain = container(&[(b"DXIL", dxil(1, &[(0x12, vec![0; 16])]))]);
        assert!(matches!(parse(&plain), Err(ShaderError::Missing(_))));
        // A native program without the stage's record.
        let wrong = blob(1, &[xbch(&payload(false, true))]);
        assert!(matches!(parse(&wrong), Err(ShaderError::Missing(_))));
        // A bad inner program is an error, not skipped.
        let mut bad = xbch(&payload(true, false));
        bad[0] = 0;
        assert!(matches!(
            parse(&blob(1, &[bad])),
            Err(ShaderError::BadMagic("XBCH"))
        ));
        // A bad signature.
        let sigless = container(&[
            (b"ISG1", vec![9]),
            (
                b"DXIL",
                dxil(1, &[(XDAT_NATIVE, xbch(&payload(true, false)))]),
            ),
        ]);
        assert!(matches!(parse(&sigless), Err(ShaderError::Truncated(_))));
    }

    #[test]
    fn fnv_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn dump_writes_blob_and_summary() {
        let dir = std::env::temp_dir().join(format!("nativ64-dump-{}", std::process::id()));
        let b = blob(1, &[xbch(&payload(true, false))]);
        let h = fnv1a64(&b);
        dump(&dir, "vertex", h, &b, &parse(&b)).unwrap();
        assert_eq!(
            std::fs::read(dir.join(format!("{h:016x}.vertex.dxbc"))).unwrap(),
            b
        );
        let t = std::fs::read_to_string(dir.join(format!("{h:016x}.vertex.txt"))).unwrap();
        assert!(t.contains("stage vertex"));
        dump(&dir, "pixel", 1, b"junk", &parse(b"junk")).unwrap();
        let t = std::fs::read_to_string(dir.join("0000000000000001.pixel.txt")).unwrap();
        assert!(t.starts_with("parse error: truncated"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
