//! Root signatures: the description the title passes to `D3D12SerializeRootSignature` /
//! `D3D12SerializeVersionedRootSignature`, its serialized form (a DXBC container holding one
//! `RTS0` chunk) and the way back, which `CreateRootSignature` will use.
//!
//! `RTS0` chunk, all little-endian `u32`, offsets counted from the start of the chunk data:
//! header (version 1 = 1.0, 2 = 1.1, 3 = 1.2; parameter count; parameter array offset, always 24;
//! static sampler count; static sampler offset; flags), then per parameter (type, shader
//! visibility, payload offset), then the payloads, then the static samplers. Payloads: table =
//! (range count, range offset), the ranges following it; constants = (register, space, count);
//! root descriptor = (register, space[, flags from 1.1]). A range is (type, count, base register,
//! space[, flags from 1.1], offset in table); a static sampler is 13 words (filter, address U/V/W,
//! mip bias, max anisotropy, comparison, border colour, min LOD, max LOD, register, space,
//! visibility) plus a flags word in 1.2. The deserializer follows the offsets, so it also reads
//! blobs that order the payloads differently.
//!
//! DXBC checksum: written, as the format wants, with the container's modified MD5 (see
//! [`dxbc_checksum`]); [`deserialize`] does not verify it. The checksum routine is checked against
//! standard MD5 for its block function but not against a blob made by Windows.
//!
//! Console flag bits (24 and up) are kept as given.

use std::ffi::c_void;

use crate::d3d12::blob;
use nativ64::com::{E_INVALIDARG, E_POINTER, HRESULT, S_OK};

/// `D3D_ROOT_SIGNATURE_VERSION`.
pub const VERSION_1_0: u32 = 1;
pub const VERSION_1_1: u32 = 2;
pub const VERSION_1_2: u32 = 3;

const PARAM_TABLE: u32 = 0;
const PARAM_CONSTANTS: u32 = 1;
const PARAM_CBV: u32 = 2;
const PARAM_UAV: u32 = 4;

const RANGE_SAMPLER: u32 = 3;
const RANGE_FLAG_DESCRIPTORS_VOLATILE: u32 = 1;
const RANGE_FLAG_DATA_VOLATILE: u32 = 2;
const DESCRIPTOR_FLAG_DATA_VOLATILE: u32 = 2;

/// One descriptor range of a table. `flags` only exists from version 1.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub kind: u32,
    pub count: u32,
    pub base_register: u32,
    pub space: u32,
    pub flags: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    Table(Vec<Range>),
    Constants {
        register: u32,
        space: u32,
        count: u32,
    },
    /// A root CBV (2), SRV (3) or UAV (4). `flags` only exists from version 1.1.
    Descriptor {
        kind: u32,
        register: u32,
        space: u32,
        flags: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub visibility: u32,
    pub binding: Binding,
}

/// A static sampler: the 13 words in the order of the serialized form (floats as raw bits, so a
/// round trip is exact), and the 1.2 flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticSampler {
    pub words: [u32; 13],
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSignature {
    pub version: u32,
    pub parameters: Vec<Parameter>,
    pub samplers: Vec<StaticSampler>,
    pub flags: u32,
}

// ---- The title's in-memory description (win64 layout) ----

/// `D3D12_ROOT_SIGNATURE_DESC` / `_DESC1` / `_DESC2`: the same head in every version.
#[repr(C)]
struct RawDesc {
    parameter_count: u32,
    parameters: *const u8,
    sampler_count: u32,
    samplers: *const u8,
    flags: u32,
}
const _: () = assert!(size_of::<RawDesc>() == 40);

/// `D3D12_VERSIONED_ROOT_SIGNATURE_DESC`: version, then the union at +8.
#[repr(C)]
struct RawVersioned {
    version: u32,
    desc: RawDesc,
}
const _: () = assert!(size_of::<RawVersioned>() == 48);
const _: () = assert!(std::mem::offset_of!(RawVersioned, desc) == 8);

/// `D3D12_ROOT_PARAMETER[1]`: type at +0, union at +8 (table: count, ranges pointer at +16;
/// constants: register, space, count; descriptor: register, space[, flags]), visibility at +24.
const RAW_PARAM_SIZE: usize = 32;
/// `D3D12_DESCRIPTOR_RANGE` (5 words) / `D3D12_DESCRIPTOR_RANGE1` (6 words).
const fn raw_range_size(version: u32) -> usize {
    if version == VERSION_1_0 {
        20
    } else {
        24
    }
}
/// `D3D12_STATIC_SAMPLER_DESC` (13 words) / `D3D12_STATIC_SAMPLER_DESC1` (14 words).
const fn raw_sampler_size(version: u32) -> usize {
    if version == VERSION_1_2 {
        56
    } else {
        52
    }
}

/// Reads `T` at `p + off`.
unsafe fn rd<T: Copy>(p: *const u8, off: usize) -> T {
    p.add(off).cast::<T>().read_unaligned()
}

/// Reads the description of `version` that `raw` describes.
///
/// # Safety
/// The pointers inside `raw` are valid for their counts, as the title promises.
unsafe fn read_desc(version: u32, raw: &RawDesc) -> Result<RootSignature, &'static str> {
    if raw.parameter_count > 0 && raw.parameters.is_null() {
        return Err("pParameters is NULL");
    }
    if raw.sampler_count > 0 && raw.samplers.is_null() {
        return Err("pStaticSamplers is NULL");
    }
    let mut parameters = Vec::new();
    for i in 0..raw.parameter_count as usize {
        let p = raw.parameters.add(i * RAW_PARAM_SIZE);
        let kind: u32 = rd(p, 0);
        let visibility: u32 = rd(p, 24);
        let binding = match kind {
            PARAM_TABLE => {
                let n: u32 = rd(p, 8);
                let ranges: *const u8 = rd(p, 16);
                if n > 0 && ranges.is_null() {
                    return Err("pDescriptorRanges is NULL");
                }
                let step = raw_range_size(version);
                let mut v = Vec::new();
                for j in 0..n as usize {
                    let r = ranges.add(j * step);
                    let (flags, offset) = if version == VERSION_1_0 {
                        (0, rd(r, 16))
                    } else {
                        (rd(r, 16), rd(r, 20))
                    };
                    v.push(Range {
                        kind: rd(r, 0),
                        count: rd(r, 4),
                        base_register: rd(r, 8),
                        space: rd(r, 12),
                        flags,
                        offset,
                    });
                }
                Binding::Table(v)
            }
            PARAM_CONSTANTS => Binding::Constants {
                register: rd(p, 8),
                space: rd(p, 12),
                count: rd(p, 16),
            },
            PARAM_CBV..=PARAM_UAV => Binding::Descriptor {
                kind,
                register: rd(p, 8),
                space: rd(p, 12),
                flags: if version == VERSION_1_0 { 0 } else { rd(p, 16) },
            },
            _ => return Err("invalid root parameter type"),
        };
        parameters.push(Parameter {
            visibility,
            binding,
        });
    }
    let mut samplers = Vec::new();
    for i in 0..raw.sampler_count as usize {
        let s = raw.samplers.add(i * raw_sampler_size(version));
        let mut words = [0u32; 13];
        for (k, w) in words.iter_mut().enumerate() {
            *w = rd(s, k * 4);
        }
        samplers.push(StaticSampler {
            words,
            flags: if version == VERSION_1_2 { rd(s, 52) } else { 0 },
        });
    }
    Ok(RootSignature {
        version,
        parameters,
        samplers,
        flags: raw.flags,
    })
}

impl RootSignature {
    /// This description at version 1.1, as `D3D12SerializeRootSignature` converts a 1.0 one:
    /// ranges are volatile (data too, except samplers), root descriptors have volatile data.
    pub fn upgraded_to_1_1(mut self) -> RootSignature {
        if self.version == VERSION_1_0 {
            self.version = VERSION_1_1;
            for p in &mut self.parameters {
                match &mut p.binding {
                    Binding::Table(ranges) => {
                        for r in ranges {
                            r.flags = RANGE_FLAG_DESCRIPTORS_VOLATILE
                                | if r.kind == RANGE_SAMPLER {
                                    0
                                } else {
                                    RANGE_FLAG_DATA_VOLATILE
                                };
                        }
                    }
                    Binding::Descriptor { flags, .. } => *flags = DESCRIPTOR_FLAG_DATA_VOLATILE,
                    Binding::Constants { .. } => {}
                }
            }
        }
        self
    }

    /// The `RTS0` chunk data.
    pub fn serialize_chunk(&self) -> Vec<u8> {
        let v = self.version;
        let mut out: Vec<u8> = Vec::new();
        let put = |out: &mut Vec<u8>, w: u32| out.extend_from_slice(&w.to_le_bytes());
        let patch = |out: &mut Vec<u8>, at: usize, w: u32| {
            out[at..at + 4].copy_from_slice(&w.to_le_bytes())
        };
        for w in [
            v,
            self.parameters.len() as u32,
            24,
            self.samplers.len() as u32,
            0,
            self.flags,
        ] {
            put(&mut out, w);
        }
        let heads = out.len();
        out.resize(heads + self.parameters.len() * 12, 0);
        for (i, p) in self.parameters.iter().enumerate() {
            let at = out.len() as u32;
            let (kind, words): (u32, Vec<u32>) = match &p.binding {
                Binding::Table(r) => (PARAM_TABLE, vec![r.len() as u32, at + 8]),
                Binding::Constants {
                    register,
                    space,
                    count,
                } => (PARAM_CONSTANTS, vec![*register, *space, *count]),
                Binding::Descriptor {
                    kind,
                    register,
                    space,
                    flags,
                } => {
                    let mut w = vec![*register, *space];
                    if v != VERSION_1_0 {
                        w.push(*flags);
                    }
                    (*kind, w)
                }
            };
            for w in words {
                put(&mut out, w);
            }
            if let Binding::Table(ranges) = &p.binding {
                for r in ranges {
                    for w in [r.kind, r.count, r.base_register, r.space] {
                        put(&mut out, w);
                    }
                    if v != VERSION_1_0 {
                        put(&mut out, r.flags);
                    }
                    put(&mut out, r.offset);
                }
            }
            let h = heads + i * 12;
            patch(&mut out, h, kind);
            patch(&mut out, h + 4, p.visibility);
            patch(&mut out, h + 8, at);
        }
        let samplers_at = out.len() as u32;
        patch(&mut out, 16, samplers_at);
        for s in &self.samplers {
            for w in s.words {
                put(&mut out, w);
            }
            if v == VERSION_1_2 {
                put(&mut out, s.flags);
            }
        }
        out
    }

    /// The DXBC container: header, one `RTS0` chunk.
    pub fn serialize(&self) -> Vec<u8> {
        let chunk = self.serialize_chunk();
        let total = 36 + 8 + chunk.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0; 16]);
        for w in [1u32, total as u32, 1, 36] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(b"RTS0");
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(&chunk);
        let sum = dxbc_checksum(&out[20..]);
        out[4..20].copy_from_slice(&sum);
        out
    }
}

/// A bounds-checked little-endian reader over the chunk data.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn u32(&self, at: usize) -> Option<u32> {
        let b = self.0.get(at..at.checked_add(4)?)?;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }
    fn words<const N: usize>(&self, at: usize) -> Option<[u32; N]> {
        let mut w = [0; N];
        for (i, x) in w.iter_mut().enumerate() {
            *x = self.u32(at.checked_add(i * 4)?)?;
        }
        Some(w)
    }
}

/// Parses an `RTS0` chunk.
pub fn deserialize_chunk(data: &[u8]) -> Option<RootSignature> {
    let r = Reader(data);
    let [version, np, params_at, ns, samplers_at, flags] = r.words::<6>(0)?;
    if !(VERSION_1_0..=VERSION_1_2).contains(&version) {
        return None;
    }
    let mut parameters = Vec::new();
    for i in 0..np as usize {
        let [kind, visibility, at] = r.words::<3>((params_at as usize).checked_add(i * 12)?)?;
        let at = at as usize;
        let binding = match kind {
            PARAM_TABLE => {
                let [n, ranges_at] = r.words::<2>(at)?;
                let step = raw_range_size(version);
                let mut v = Vec::new();
                for j in 0..n as usize {
                    let a = (ranges_at as usize).checked_add(j * step)?;
                    let [k, count, base_register, space] = r.words::<4>(a)?;
                    let (flags, offset) = if version == VERSION_1_0 {
                        (0, r.u32(a + 16)?)
                    } else {
                        (r.u32(a + 16)?, r.u32(a + 20)?)
                    };
                    v.push(Range {
                        kind: k,
                        count,
                        base_register,
                        space,
                        flags,
                        offset,
                    });
                }
                Binding::Table(v)
            }
            PARAM_CONSTANTS => {
                let [register, space, count] = r.words::<3>(at)?;
                Binding::Constants {
                    register,
                    space,
                    count,
                }
            }
            PARAM_CBV..=PARAM_UAV => {
                let [register, space] = r.words::<2>(at)?;
                Binding::Descriptor {
                    kind,
                    register,
                    space,
                    flags: if version == VERSION_1_0 {
                        0
                    } else {
                        r.u32(at + 8)?
                    },
                }
            }
            _ => return None,
        };
        parameters.push(Parameter {
            visibility,
            binding,
        });
    }
    let mut samplers = Vec::new();
    for i in 0..ns as usize {
        let a = (samplers_at as usize).checked_add(i * raw_sampler_size(version))?;
        samplers.push(StaticSampler {
            words: r.words::<13>(a)?,
            flags: if version == VERSION_1_2 {
                r.u32(a + 52)?
            } else {
                0
            },
        });
    }
    Some(RootSignature {
        version,
        parameters,
        samplers,
        flags,
    })
}

/// Parses a serialized root signature (DXBC container with an `RTS0` chunk). The checksum is not
/// verified.
pub fn deserialize(blob: &[u8]) -> Option<RootSignature> {
    let r = Reader(blob);
    if blob.get(..4)? != b"DXBC" {
        return None;
    }
    let total = r.u32(24)? as usize;
    let chunks = r.u32(28)? as usize;
    let data = blob.get(..total)?;
    for i in 0..chunks {
        let at = r.u32(32usize.checked_add(i * 4)?)? as usize;
        let size = r.u32(at.checked_add(4)?)? as usize;
        if data.get(at..at.checked_add(4)?)? == b"RTS0" {
            let start = at.checked_add(8)?;
            return deserialize_chunk(data.get(start..start.checked_add(size)?)?);
        }
    }
    None
}

// ---- DXBC checksum ----

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let m: [u32; 16] =
        std::array::from_fn(|i| u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap()));
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32;
        let t = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(m[g])
            .rotate_left(S[(i / 16) * 4 + i % 4]);
        (a, d, c) = (d, c, b);
        b = b.wrapping_add(t);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

/// The DXBC container checksum over everything after the checksum field (`data` = the container
/// from byte 20): MD5's block function and initial state, with the container's own padding (the
/// bit count goes in the first word of the last block, `bytes * 2 | 1` in its last word, and a
/// remainder of 56 bytes or more spills into an extra block).
pub fn dxbc_checksum(data: &[u8]) -> [u8; 16] {
    let mut st = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let (blocks, rest) = data.as_chunks::<64>();
    for b in blocks {
        md5_block(&mut st, b);
    }
    let bits = (data.len() as u32).wrapping_mul(8);
    let tag = ((data.len() as u32).wrapping_mul(2)) | 1;
    let mut last = [0u8; 64];
    if rest.len() >= 56 {
        last[..rest.len()].copy_from_slice(rest);
        last[rest.len()] = 0x80;
        md5_block(&mut st, &last);
        last = [0; 64];
        last[..4].copy_from_slice(&bits.to_le_bytes());
    } else {
        last[..4].copy_from_slice(&bits.to_le_bytes());
        last[4..4 + rest.len()].copy_from_slice(rest);
        last[4 + rest.len()] = 0x80;
    }
    last[60..].copy_from_slice(&tag.to_le_bytes());
    md5_block(&mut st, &last);
    let mut out = [0u8; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(st) {
        *o = s.to_le_bytes();
    }
    out
}

// ---- Exports ----

/// Hands `bytes` out as a blob through `out`; returns the HRESULT.
unsafe fn give(out: *mut *mut c_void, bytes: Vec<u8>) {
    if !out.is_null() {
        *out = blob::create(bytes);
    }
}

unsafe fn fail(error: *mut *mut c_void, message: &str) -> HRESULT {
    let mut text = message.as_bytes().to_vec();
    text.push(0);
    give(error, text);
    E_INVALIDARG
}

/// `D3D12SerializeVersionedRootSignature(desc, *blob, *error)`.
///
/// # Safety
/// `desc` is NULL or a `D3D12_VERSIONED_ROOT_SIGNATURE_DESC` whose pointers are valid; `blob` and
/// `error` are NULL or writable.
pub unsafe fn serialize_versioned(
    desc: *const c_void,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    if !blob.is_null() {
        *blob = std::ptr::null_mut();
    }
    if !error.is_null() {
        *error = std::ptr::null_mut();
    }
    let Some(raw) = (desc as *const RawVersioned).as_ref() else {
        return E_INVALIDARG;
    };
    if blob.is_null() {
        return E_POINTER;
    }
    if !(VERSION_1_0..=VERSION_1_2).contains(&raw.version) {
        return fail(error, "unsupported root signature version");
    }
    match read_desc(raw.version, &raw.desc) {
        Ok(sig) => {
            give(blob, sig.serialize());
            S_OK
        }
        Err(m) => fail(error, m),
    }
}

/// `D3D12SerializeRootSignature(desc, version, *blob, *error)`: `desc` is a 1.0 description,
/// `version` the form to serialize (1.0 or 1.1).
///
/// # Safety
/// As [`serialize_versioned`], with a `D3D12_ROOT_SIGNATURE_DESC`.
pub unsafe fn serialize(
    desc: *const c_void,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    if !blob.is_null() {
        *blob = std::ptr::null_mut();
    }
    if !error.is_null() {
        *error = std::ptr::null_mut();
    }
    let Some(raw) = (desc as *const RawDesc).as_ref() else {
        return E_INVALIDARG;
    };
    if blob.is_null() {
        return E_POINTER;
    }
    if version != VERSION_1_0 && version != VERSION_1_1 {
        return fail(error, "unsupported root signature version");
    }
    match read_desc(VERSION_1_0, raw) {
        Ok(sig) => {
            let sig = if version == VERSION_1_1 {
                sig.upgraded_to_1_1()
            } else {
                sig
            };
            give(blob, sig.serialize());
            S_OK
        }
        Err(m) => fail(error, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(version: u32) -> RootSignature {
        let v1 = version != VERSION_1_0;
        let range = |kind, count, offset| Range {
            kind,
            count,
            base_register: 2,
            space: 1,
            flags: if v1 { 5 } else { 0 },
            offset,
        };
        RootSignature {
            version,
            flags: 0x0100_0001,
            parameters: vec![
                Parameter {
                    visibility: 0,
                    binding: Binding::Table(vec![range(0, 4, 0), range(3, 1, 4)]),
                },
                Parameter {
                    visibility: 5,
                    binding: Binding::Constants {
                        register: 3,
                        space: 0,
                        count: 16,
                    },
                },
                Parameter {
                    visibility: 1,
                    binding: Binding::Descriptor {
                        kind: 3,
                        register: 7,
                        space: 2,
                        flags: if v1 { 2 } else { 0 },
                    },
                },
            ],
            samplers: vec![
                StaticSampler {
                    words: [21, 1, 1, 1, 0x3f80_0000, 16, 4, 0, 0, 0x7f7f_ffff, 0, 0, 0],
                    flags: if version == VERSION_1_2 { 1 } else { 0 },
                },
                StaticSampler {
                    words: [1; 13],
                    flags: 0,
                },
            ],
        }
    }

    #[test]
    fn md5_block_matches_standard() {
        let mut st = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        let mut block = [0u8; 64];
        block[0] = 0x80;
        md5_block(&mut st, &block);
        let hex: String = st
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn round_trips_every_version() {
        for v in [VERSION_1_0, VERSION_1_1, VERSION_1_2] {
            let s = sample(v);
            let blob = s.serialize();
            assert_eq!(&blob[..4], b"DXBC");
            assert_eq!(
                u32::from_le_bytes(blob[24..28].try_into().unwrap()) as usize,
                blob.len()
            );
            assert_eq!(&blob[4..20], &dxbc_checksum(&blob[20..]));
            assert_eq!(deserialize(&blob), Some(s));
        }
    }

    #[test]
    fn rejects_garbage() {
        let blob = sample(VERSION_1_1).serialize();
        assert_eq!(deserialize(&blob[..blob.len() - 1]), None);
        assert_eq!(deserialize(b"DXBC"), None);
        let mut bad = blob.clone();
        bad[0] = b'X';
        assert_eq!(deserialize(&bad), None);
        for cut in 0..blob.len() {
            let _ = deserialize(&blob[..cut]);
        }
    }

    /// Builds the title's memory for a 1.1 description and serializes it through the export.
    #[test]
    fn exports_serialize_in_memory_descriptions() {
        // Table of one 1.1 range, one root constants parameter, one static sampler.
        let range: [u32; 6] = [0, 2, 1, 0, 3, 0xffff_ffff];
        let mut params = [0u8; 64];
        params[0..4].copy_from_slice(&PARAM_TABLE.to_le_bytes());
        params[8..12].copy_from_slice(&1u32.to_le_bytes());
        params[16..24].copy_from_slice(&(range.as_ptr() as u64).to_le_bytes());
        params[24..28].copy_from_slice(&4u32.to_le_bytes());
        params[32..36].copy_from_slice(&PARAM_CONSTANTS.to_le_bytes());
        params[40..44].copy_from_slice(&9u32.to_le_bytes());
        params[44..48].copy_from_slice(&1u32.to_le_bytes());
        params[48..52].copy_from_slice(&4u32.to_le_bytes());
        let sampler = [7u32; 13];
        let raw = RawVersioned {
            version: VERSION_1_1,
            desc: RawDesc {
                parameter_count: 2,
                parameters: params.as_ptr(),
                sampler_count: 1,
                samplers: sampler.as_ptr().cast(),
                flags: 0x11,
            },
        };
        let mut blob = std::ptr::null_mut();
        let mut err = std::ptr::dangling_mut::<c_void>();
        let hr =
            unsafe { serialize_versioned(&raw as *const _ as *const c_void, &mut blob, &mut err) };
        assert_eq!(hr, S_OK);
        assert!(err.is_null());
        let sig = deserialize(unsafe { crate::d3d12::blob::bytes(blob) }).unwrap();
        assert_eq!(sig.version, VERSION_1_1);
        assert_eq!(sig.flags, 0x11);
        assert_eq!(
            sig.parameters,
            vec![
                Parameter {
                    visibility: 4,
                    binding: Binding::Table(vec![Range {
                        kind: 0,
                        count: 2,
                        base_register: 1,
                        space: 0,
                        flags: 3,
                        offset: 0xffff_ffff
                    }])
                },
                Parameter {
                    visibility: 0,
                    binding: Binding::Constants {
                        register: 9,
                        space: 1,
                        count: 4
                    }
                },
            ]
        );
        assert_eq!(
            sig.samplers,
            vec![StaticSampler {
                words: [7; 13],
                flags: 0
            }]
        );
    }

    #[test]
    fn version_1_0_export_and_upgrade() {
        let range: [u32; 5] = [3, 1, 0, 0, 0];
        let mut param = [0u8; 32];
        param[8..12].copy_from_slice(&1u32.to_le_bytes());
        param[16..24].copy_from_slice(&(range.as_ptr() as u64).to_le_bytes());
        let raw = RawDesc {
            parameter_count: 1,
            parameters: param.as_ptr(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 0,
        };
        for (version, flags) in [
            (VERSION_1_0, 0),
            (VERSION_1_1, RANGE_FLAG_DESCRIPTORS_VOLATILE),
        ] {
            let mut blob = std::ptr::null_mut();
            let hr = unsafe {
                serialize(
                    &raw as *const _ as *const c_void,
                    version,
                    &mut blob,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(hr, S_OK);
            let sig = deserialize(unsafe { crate::d3d12::blob::bytes(blob) }).unwrap();
            assert_eq!(sig.version, version);
            let Binding::Table(r) = &sig.parameters[0].binding else {
                panic!()
            };
            assert_eq!(r[0].flags, flags);
        }
    }

    #[test]
    fn error_cases() {
        let raw = RawVersioned {
            version: 9,
            desc: RawDesc {
                parameter_count: 0,
                parameters: std::ptr::null(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 0,
            },
        };
        let p = &raw as *const _ as *const c_void;
        let mut blob = std::ptr::dangling_mut::<c_void>();
        let mut err = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                serialize_versioned(std::ptr::null(), &mut blob, &mut err),
                E_INVALIDARG
            );
            assert!(blob.is_null());
            assert_eq!(serialize_versioned(p, &mut blob, &mut err), E_INVALIDARG);
            assert!(blob.is_null() && !err.is_null());
            assert!(crate::d3d12::blob::bytes(err).ends_with(&[0]));
            assert_eq!(
                serialize_versioned(p, std::ptr::null_mut(), std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                serialize(std::ptr::null(), 1, &mut blob, &mut err),
                E_INVALIDARG
            );
            let d = &raw.desc as *const _ as *const c_void;
            assert_eq!(
                serialize(d, 3, &mut blob, std::ptr::null_mut()),
                E_INVALIDARG
            );
            // An empty 1.0 description is valid.
            assert_eq!(serialize(d, 1, &mut blob, std::ptr::null_mut()), S_OK);
            // A bad parameter type is rejected.
            let bad = [0u8; 32];
            let mut bad = bad;
            bad[0] = 9;
            let raw2 = RawDesc {
                parameter_count: 1,
                parameters: bad.as_ptr(),
                sampler_count: 0,
                samplers: std::ptr::null(),
                flags: 0,
            };
            assert_eq!(
                serialize(
                    &raw2 as *const _ as *const c_void,
                    1,
                    &mut blob,
                    std::ptr::null_mut()
                ),
                E_INVALIDARG
            );
        }
    }
}
