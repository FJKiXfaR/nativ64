//! Pipeline state objects (`ID3D12PipelineState`), made by the device's four creation slots:
//! `CreateGraphicsPipelineState` (10), `CreateComputePipelineState` (11), the console's
//! `CreateComputePipelineStateX` (62, `(desc, extension count, extensions, iid, out)`) and
//! `CreatePipelineState` from a stream (86).
//!
//! Nothing is translated or compiled here (milestone G4): a pipeline keeps a copy of each stage's
//! bytecode, in description order, and what fixed-function state is cheap to read. Everything the
//! title points at is copied (shader code, input element names), since it may free it after the
//! call. The root signature is kept as a clone of its parsed model, not a reference to its
//! object: the title may release the object at once, and the model is plain data.
//!
//! The descriptions are the PC ones (graphics 0x298 bytes, compute 0x38); the stream's subobject
//! types are PC's too except that the console numbers amplification and mesh shaders 23 and 24
//! (PC 24 and 25). The extensions of the X variant (console shader settings) are ignored. A
//! stream subobject of a type we do not size is `E_INVALIDARG`, logged, so a new one shows up.

use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::d3d12::rootobj::RootSignatureData;
use crate::d3d12::rootsig::RootSignature;
use crate::xobj::{Guid, Iface, SlotTable, XObject, HRESULT};

pub static PIPELINE_IIDS: [Guid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// Size of `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.
const GRAPHICS_DESC_SIZE: usize = 0x298;
/// Bytes of `D3D12_COMPUTE_PIPELINE_STATE_DESC` we read (root signature and shader; the node
/// mask, cached blob and flags follow).
const COMPUTE_DESC_READ: usize = 24;
/// Offsets in the graphics description.
mod gfx {
    pub const STAGES: usize = 8; // five `D3D12_SHADER_BYTECODE`s
    pub const INPUT_LAYOUT: usize = 560; // elements pointer, count
    pub const TOPOLOGY: usize = 580;
    pub const NUM_TARGETS: usize = 584;
    pub const TARGET_FORMATS: usize = 588;
    pub const DEPTH_FORMAT: usize = 620;
    pub const SAMPLES: usize = 624; // count, quality
}
/// Stage names, in graphics description order.
const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];
/// Most input elements D3D12 allows.
const MAX_INPUT_ELEMENTS: usize = 32;

/// One stage's bytecode, copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

/// `D3D12_INPUT_ELEMENT_DESC`, its name copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputElement {
    pub name: String,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub class: u32,
    pub step_rate: u32,
}

/// The fixed-function state read from a graphics description or stream. Blend, rasterizer and
/// depth-stencil state stay in `raw` (the description or stream bytes; its pointers are the
/// title's and must not be followed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphicsState {
    pub target_formats: Vec<u32>,
    pub depth_format: u32,
    /// `D3D12_PRIMITIVE_TOPOLOGY_TYPE`.
    pub topology_type: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub input_layout: Vec<InputElement>,
    pub raw: Vec<u8>,
}

/// Console fields from `+0x08`, then the model (ours, not read by the title).
#[repr(C)]
pub struct PipelineData {
    bits: u32,
    console_refs: u32,
    /// `+0x10`: the console's inline command packet, left zero.
    packet: [u32; 8],
    /// `None`: no root signature in the description (the shader's own is used).
    pub root: Option<RootSignature>,
    pub stages: Vec<Stage>,
    /// `Some` for graphics pipelines.
    pub graphics: Option<GraphicsState>,
}
const _: () = assert!(std::mem::offset_of!(PipelineData, packet) == 8);

static TABLE: SlotTable<8> =
    with_child_methods(SlotTable::new(Iface::PipelineState).with_iunknown::<PipelineData>());

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// The root signature a description names, if it names one of our objects.
///
/// # Safety
/// `ptr` is NULL or a live console object.
unsafe fn root_of(ptr: u64) -> Option<RootSignature> {
    let obj = ptr as *mut c_void;
    if obj.is_null() {
        return None;
    }
    if XObject::<RootSignatureData>::prefix(obj).iface() != Iface::RootSignature {
        eprintln!("nativ64: pipeline description names {obj:p}, which is not a root signature");
        return None;
    }
    Some(XObject::<RootSignatureData>::data(obj).model.clone())
}

/// The stage at `at` in `b` (`D3D12_SHADER_BYTECODE`: pointer, size), copied; `None` if empty.
///
/// # Safety
/// The pointer names `size` readable bytes, or is NULL.
unsafe fn stage(name: &'static str, b: &[u8], at: usize) -> Option<Stage> {
    let (ptr, size) = (u64_at(b, at) as *const u8, u64_at(b, at + 8) as usize);
    if ptr.is_null() || size == 0 {
        return None;
    }
    Some(Stage {
        name,
        bytes: std::slice::from_raw_parts(ptr, size).to_vec(),
    })
}

/// Input elements, names copied.
///
/// # Safety
/// `ptr` names `count` elements, or is NULL.
unsafe fn input_layout(ptr: u64, count: u32) -> Vec<InputElement> {
    let count = (count as usize).min(MAX_INPUT_ELEMENTS);
    if ptr == 0 {
        return Vec::new();
    }
    let raw = std::slice::from_raw_parts(ptr as *const u8, count * 32);
    raw.as_chunks::<32>()
        .0
        .iter()
        .map(|e| {
            let name = u64_at(e, 0) as *const std::ffi::c_char;
            InputElement {
                name: if name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned()
                },
                index: u32_at(e, 8),
                format: u32_at(e, 12),
                slot: u32_at(e, 16),
                offset: u32_at(e, 20),
                class: u32_at(e, 24),
                step_rate: u32_at(e, 28),
            }
        })
        .collect()
}

/// Parses each stage blob (a failure is logged once per blob, never fatal) and feeds the shader
/// dump (`NATIV64_DUMP_SHADERS`).
fn observe_shaders(data: &PipelineData) -> usize {
    for s in &data.stages {
        crate::shader::observe(s.name, &s.bytes);
    }
    let stages: Vec<(&str, &[u8])> = data.stages.iter().map(|s| (s.name, &s.bytes[..])).collect();
    let targets = data
        .graphics
        .as_ref()
        .map_or(&[][..], |g| &g.target_formats[..]);
    crate::shader::note_pipeline(&stages, targets)
}

fn make(data: PipelineData, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    let id = observe_shaders(&data);
    // SAFETY: `iid` and `out` are the title's, NULL tolerated.
    let hr = unsafe {
        hand_out(
            &TABLE,
            Iface::PipelineState,
            &PIPELINE_IIDS,
            || data,
            iid,
            out,
        )
    };
    // SAFETY: `out` is NULL or was just written by `hand_out`.
    if let Some(obj) = unsafe { out.as_ref() }.filter(|o| !o.is_null()) {
        crate::shader::bind_pipeline_object(*obj as usize, id);
    }
    hr
}

fn data(
    root: Option<RootSignature>,
    stages: Vec<Stage>,
    graphics: Option<GraphicsState>,
) -> PipelineData {
    PipelineData {
        bits: 0,
        console_refs: 0,
        packet: [0; 8],
        root,
        stages,
        graphics,
    }
}

/// An error result with `*out` cleared.
unsafe fn fail(out: *mut *mut c_void) -> HRESULT {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_INVALIDARG
}

/// `ID3D12Device::CreateGraphicsPipelineState(desc, riid, out)` (device slot 10).
///
/// # Safety
/// `desc` is NULL or a graphics description whose pointers are valid; `iid`, `out` NULL or valid.
pub unsafe extern "win64" fn create_graphics(
    _this: *mut c_void,
    desc: *const u8,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if desc.is_null() {
        return fail(out);
    }
    let raw = std::slice::from_raw_parts(desc, GRAPHICS_DESC_SIZE);
    let stages = GRAPHICS_STAGES
        .iter()
        .enumerate()
        .filter_map(|(i, name)| stage(name, raw, gfx::STAGES + 16 * i))
        .collect();
    let targets = (u32_at(raw, gfx::NUM_TARGETS) as usize).min(8);
    let state = GraphicsState {
        target_formats: (0..targets)
            .map(|i| u32_at(raw, gfx::TARGET_FORMATS + 4 * i))
            .collect(),
        depth_format: u32_at(raw, gfx::DEPTH_FORMAT),
        topology_type: u32_at(raw, gfx::TOPOLOGY),
        sample_count: u32_at(raw, gfx::SAMPLES),
        sample_quality: u32_at(raw, gfx::SAMPLES + 4),
        input_layout: input_layout(
            u64_at(raw, gfx::INPUT_LAYOUT),
            u32_at(raw, gfx::INPUT_LAYOUT + 8),
        ),
        raw: raw.to_vec(),
    };
    make(data(root_of(u64_at(raw, 0)), stages, Some(state)), iid, out)
}

/// `ID3D12Device::CreateComputePipelineState(desc, riid, out)` (device slot 11).
///
/// # Safety
/// `desc` is NULL or a compute description whose pointers are valid; `iid`, `out` NULL or valid.
pub unsafe extern "win64" fn create_compute(
    _this: *mut c_void,
    desc: *const u8,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if desc.is_null() {
        return fail(out);
    }
    let raw = std::slice::from_raw_parts(desc, COMPUTE_DESC_READ);
    let Some(cs) = stage("compute", raw, 8) else {
        eprintln!("nativ64: CreateComputePipelineState: no shader bytecode");
        return fail(out);
    };
    make(data(root_of(u64_at(raw, 0)), vec![cs], None), iid, out)
}

/// The console's `CreateComputePipelineStateX(desc, extension count, extensions, riid, out)`
/// (device slot 62, `+0x1f0`): the plain description plus console shader settings, ignored.
///
/// # Safety
/// As [`create_compute`]; the extensions are not read.
pub unsafe extern "win64" fn create_compute_x(
    this: *mut c_void,
    desc: *const u8,
    _extension_count: u32,
    _extensions: *const c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    create_compute(this, desc, iid, out)
}

/// `E_NOTIMPL`.
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
/// `E_ACCESSDENIED`.
const E_ACCESSDENIED: HRESULT = 0x8007_0005_u32 as i32;

/// The console's `Deserialize{Graphics,Compute}PipelineStateX(state, riid, out)` (device slots 57
/// and 67): the cache is the console driver's format, so `*out` is cleared and `E_NOTIMPL` makes
/// the title create the pipeline from its description. Warns once.
///
/// # Safety
/// `out` is NULL or writable.
pub unsafe extern "win64" fn deserialize_x(
    _this: *mut c_void,
    _state: *const u8,
    _iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!("nativ64: DeserializePipelineStateX: console pipeline caches are not supported");
    });
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_NOTIMPL
}

/// The console's `SerializePipelineStateX(pso, ...)` (device slots 56 and 66): not available in a
/// release runtime, `E_ACCESSDENIED`.
///
/// # Safety
/// Nothing is read.
pub unsafe extern "win64" fn serialize_x(_this: *mut c_void, _pso: *mut c_void) -> HRESULT {
    E_ACCESSDENIED
}

/// The console's `SetDebugFlagsX(flags)` (device slot 52): accepted and ignored.
///
/// # Safety
/// Nothing is read.
pub unsafe extern "win64" fn set_debug_flags_x(_this: *mut c_void, _flags: u32) -> HRESULT {
    0
}

/// A stream subobject type's size (type included), and the stage it is if it is a shader.
fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let shader = |name| Some((24, Some(name)));
    match kind {
        1 => shader("vertex"),
        2 => shader("pixel"),
        3 => shader("domain"),
        4 => shader("hull"),
        5 => shader("geometry"),
        6 => shader("compute"),
        23 => shader("amplification"),
        24 => shader("mesh"),
        0 | 17 => Some((16, None)),  // root signature, sample description
        12 | 19 => Some((24, None)), // input layout, cached blob
        7 | 15 => Some((40, None)),  // stream output, render target formats
        8 => Some((336, None)),      // blend
        10 => Some((48, None)),      // rasterizer
        11 => Some((56, None)),      // depth-stencil
        21 => Some((64, None)),      // depth-stencil 1
        22 => Some((32, None)),      // view instancing
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)), // mask, strip cut, topology, depth format, node mask, flags
        _ => None,
    }
}

/// `ID3D12Device2::CreatePipelineState(desc, riid, out)` with `{ size, stream }` (device slot 86).
///
/// # Safety
/// `desc` is NULL or a stream description naming `size` readable bytes whose pointers are valid;
/// `iid`, `out` NULL or valid.
pub unsafe extern "win64" fn create_stream(
    _this: *mut c_void,
    desc: *const [usize; 2],
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(&[size, ptr]) = desc.as_ref() else {
        return fail(out);
    };
    if ptr == 0 || size == 0 {
        return fail(out);
    }
    let s = std::slice::from_raw_parts(ptr as *const u8, size);
    let (mut root, mut stages, mut state) = (None, Vec::new(), GraphicsState::default());
    let mut is_graphics = false;
    let mut at = 0;
    while at < s.len() {
        let kind = if at + 4 <= s.len() {
            u32_at(s, at)
        } else {
            u32::MAX
        };
        let Some((len, name)) = subobject(kind) else {
            eprintln!(
                "nativ64: CreatePipelineState: unknown stream subobject type {kind} at {at:#x}"
            );
            return fail(out);
        };
        if at + len > s.len() {
            eprintln!(
                "nativ64: CreatePipelineState: subobject {kind} at {at:#x} runs past the stream"
            );
            return fail(out);
        }
        let o = &s[at..at + len];
        match (kind, name) {
            (_, Some(name)) => {
                stages.extend(stage(name, o, 8));
                is_graphics |= name != "compute";
            }
            (0, _) => root = root_of(u64_at(o, 8)),
            (12, _) => {
                state.input_layout = input_layout(u64_at(o, 8), u32_at(o, 16));
                is_graphics = true;
            }
            (14, _) => state.topology_type = u32_at(o, 4),
            (15, _) => {
                let n = (u32_at(o, 36) as usize).min(8);
                state.target_formats = (0..n).map(|i| u32_at(o, 4 + 4 * i)).collect();
            }
            (16, _) => state.depth_format = u32_at(o, 4),
            (17, _) => {
                state.sample_count = u32_at(o, 4);
                state.sample_quality = u32_at(o, 8);
            }
            _ => {}
        }
        at += len;
    }
    if !is_graphics && stages.is_empty() {
        eprintln!("nativ64: CreatePipelineState: stream without shader");
        return fail(out);
    }
    let graphics = is_graphics.then(|| GraphicsState {
        raw: s.to_vec(),
        ..state
    });
    make(data(root, stages, graphics), iid, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{E_NOINTERFACE, S_OK};

    const N: *mut c_void = std::ptr::null_mut();

    fn put(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    unsafe fn pipe<'a>(p: *mut c_void) -> &'a PipelineData {
        XObject::<PipelineData>::data(p)
    }

    fn root() -> (*mut c_void, Vec<u8>) {
        let blob = RootSignature {
            version: 1,
            parameters: vec![],
            samplers: vec![],
            flags: 5,
        }
        .serialize();
        let mut r = N;
        let hr = unsafe {
            crate::d3d12::rootobj::create_root_signature(
                N,
                0,
                blob.as_ptr(),
                blob.len(),
                &crate::d3d12::rootobj::ROOT_SIGNATURE_IIDS[0],
                &mut r,
            )
        };
        assert_eq!(hr, S_OK);
        (r, blob)
    }

    #[test]
    fn console_cache_and_debug_slots() {
        let mut out = 0x1234 as *mut c_void;
        unsafe {
            assert_eq!(
                deserialize_x(N, std::ptr::null(), std::ptr::null(), &mut out),
                E_NOTIMPL
            );
            assert!(out.is_null());
            assert_eq!(
                deserialize_x(N, std::ptr::null(), std::ptr::null(), std::ptr::null_mut()),
                E_NOTIMPL
            );
            assert_eq!(serialize_x(N, N), E_ACCESSDENIED);
            assert_eq!(set_debug_flags_x(N, 7), 0);
        }
    }

    #[test]
    fn compute_keeps_code_and_root() {
        let (r, _blob) = root();
        let code = [1u8, 2, 3, 4];
        let mut desc = [0u8; 56];
        put(&mut desc, 0, r as u64);
        put(&mut desc, 8, code.as_ptr() as u64);
        put(&mut desc, 16, code.len() as u64);
        unsafe {
            let mut p = N;
            assert_eq!(
                create_compute(N, desc.as_ptr(), &PIPELINE_IIDS[0], &mut p),
                S_OK
            );
            let d = pipe(p);
            assert_eq!(
                d.stages,
                [Stage {
                    name: "compute",
                    bytes: code.to_vec()
                }]
            );
            assert_eq!(d.root.as_ref().unwrap().flags, 5);
            assert!(d.graphics.is_none());
            assert_eq!(
                XObject::<PipelineData>::prefix(p).iface(),
                Iface::PipelineState
            );
            assert_eq!(XObject::<PipelineData>::release(p), 0);
            // The X variant reads the same description and ignores the extensions.
            let mut q = N;
            assert_eq!(
                create_compute_x(
                    N,
                    desc.as_ptr(),
                    3,
                    0x10 as *const _,
                    &PIPELINE_IIDS[1],
                    &mut q
                ),
                S_OK
            );
            assert_eq!(pipe(q).stages[0].bytes, code);
            // Without a root signature: None.
            put(&mut desc, 0, 0);
            let mut z = N;
            assert_eq!(
                create_compute(N, desc.as_ptr(), &PIPELINE_IIDS[0], &mut z),
                S_OK
            );
            assert!(pipe(z).root.is_none());
        }
    }

    #[test]
    fn creation_errors() {
        unsafe {
            let mut p = std::ptr::dangling_mut();
            assert_eq!(
                create_compute(N, std::ptr::null(), &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            assert!(p.is_null());
            p = std::ptr::dangling_mut();
            assert_eq!(
                create_graphics(N, std::ptr::null(), &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            assert!(p.is_null());
            assert_eq!(
                create_compute_x(
                    N,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &PIPELINE_IIDS[0],
                    &mut p
                ),
                E_INVALIDARG
            );
            assert_eq!(
                create_stream(N, std::ptr::null(), &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            // A compute description without code.
            let desc = [0u8; 56];
            assert_eq!(
                create_compute(N, desc.as_ptr(), &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            // Unknown interface; no output pointer.
            let code = [1u8];
            let mut d = [0u8; 56];
            put(&mut d, 8, code.as_ptr() as u64);
            put(&mut d, 16, 1);
            let other = guid(1, 2, 3, [4; 8]);
            assert_eq!(create_compute(N, d.as_ptr(), &other, &mut p), E_NOINTERFACE);
            assert_eq!(
                create_compute(N, d.as_ptr(), &PIPELINE_IIDS[0], std::ptr::null_mut()),
                S_FALSE
            );
        }
    }

    #[test]
    fn graphics_keeps_stages_and_state() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let name = c"TEXCOORD";
        let mut el = [0u8; 32];
        put(&mut el, 0, name.as_ptr() as u64);
        for (i, w) in [2u32, 16, 1, 8, 0, 0].iter().enumerate() {
            put32(&mut el, 8 + 4 * i, *w);
        }
        let (r, _blob) = root();
        let mut desc = vec![0u8; GRAPHICS_DESC_SIZE];
        put(&mut desc, 0, r as u64);
        put(&mut desc, 8, vs.as_ptr() as u64);
        put(&mut desc, 16, 3);
        put(&mut desc, 24, ps.as_ptr() as u64);
        put(&mut desc, 32, 5);
        put(&mut desc, gfx::INPUT_LAYOUT, el.as_ptr() as u64);
        put32(&mut desc, gfx::INPUT_LAYOUT + 8, 1);
        put32(&mut desc, gfx::TOPOLOGY, 3);
        put32(&mut desc, gfx::NUM_TARGETS, 2);
        put32(&mut desc, gfx::TARGET_FORMATS, 28);
        put32(&mut desc, gfx::TARGET_FORMATS + 4, 87);
        put32(&mut desc, gfx::DEPTH_FORMAT, 40);
        put32(&mut desc, gfx::SAMPLES, 4);
        unsafe {
            let mut p = N;
            assert_eq!(
                create_graphics(N, desc.as_ptr(), &PIPELINE_IIDS[0], &mut p),
                S_OK
            );
            let d = pipe(p);
            assert_eq!(d.stages.len(), 2);
            assert_eq!(
                (d.stages[0].name, &d.stages[0].bytes[..]),
                ("vertex", &vs[..])
            );
            assert_eq!(
                (d.stages[1].name, &d.stages[1].bytes[..]),
                ("pixel", &ps[..])
            );
            let g = d.graphics.as_ref().unwrap();
            assert_eq!(g.target_formats, [28, 87]);
            assert_eq!(
                (g.depth_format, g.topology_type, g.sample_count),
                (40, 3, 4)
            );
            assert_eq!(g.input_layout.len(), 1);
            assert_eq!(g.input_layout[0].name, "TEXCOORD");
            assert_eq!(
                (
                    g.input_layout[0].index,
                    g.input_layout[0].format,
                    g.input_layout[0].slot,
                    g.input_layout[0].offset
                ),
                (2, 16, 1, 8)
            );
            assert_eq!(g.raw.len(), GRAPHICS_DESC_SIZE);
            assert_eq!(d.root.as_ref().unwrap().flags, 5);
        }
    }

    /// The console's stream layout: root signature, five shader slots (the first a mesh shader),
    /// blend, sample mask, rasterizer, depth-stencil 1, sample description, input layout,
    /// topology, render target and depth formats: 0x2b0 bytes.
    fn stream(ms: &[u8], ps: &[u8], name: &std::ffi::CStr, root: u64) -> Vec<u8> {
        let mut s = vec![0u8; 0x2b0];
        for (at, kind) in [
            (0x00, 0),
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put32(&mut s, at, kind);
        }
        put(&mut s, 0x08, root);
        put(&mut s, 0x18, ms.as_ptr() as u64);
        put(&mut s, 0x20, ms.len() as u64);
        put(&mut s, 0x30, ps.as_ptr() as u64);
        put(&mut s, 0x38, ps.len() as u64);
        put32(&mut s, 0x254, 2); // sample count
        put32(&mut s, 0x27c, 3); // topology type
        put32(&mut s, 0x284, 28); // target 0
        put32(&mut s, 0x2a4, 1); // target count
        put32(&mut s, 0x2ac, 40); // depth format
                                  // One input element, "name", at the end of the buffer's heap copy below.
        let el: &'static mut [u8; 32] = Box::leak(Box::new([0u8; 32]));
        put(el, 0, name.as_ptr() as u64);
        put32(el, 8, 7);
        put(&mut s, 0x268, el.as_ptr() as u64);
        put32(&mut s, 0x270, 1);
        s
    }

    #[test]
    fn streams_yield_their_stages_and_state() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let (r, _blob) = root();
        let s = stream(&ms, &ps, c"POSITION", r as u64);
        let desc = [s.len(), s.as_ptr() as usize];
        unsafe {
            let mut p = N;
            assert_eq!(create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p), S_OK);
            let d = pipe(p);
            assert_eq!(d.stages.len(), 2);
            assert_eq!(
                (d.stages[0].name, &d.stages[0].bytes[..]),
                ("mesh", &ms[..])
            );
            assert_eq!(
                (d.stages[1].name, &d.stages[1].bytes[..]),
                ("pixel", &ps[..])
            );
            let g = d.graphics.as_ref().unwrap();
            assert_eq!(g.target_formats, [28]);
            assert_eq!(
                (g.depth_format, g.topology_type, g.sample_count),
                (40, 3, 2)
            );
            assert_eq!(g.input_layout[0].name, "POSITION");
            assert_eq!(g.input_layout[0].index, 7);
            assert_eq!(g.raw, s);
            assert_eq!(d.root.as_ref().unwrap().flags, 5);
        }
    }

    #[test]
    fn compute_stream() {
        let cs = [8u8; 3];
        let mut s = vec![0u8; 24];
        put32(&mut s, 0, 6);
        put(&mut s, 8, cs.as_ptr() as u64);
        put(&mut s, 16, 3);
        let desc = [s.len(), s.as_ptr() as usize];
        unsafe {
            let mut p = N;
            assert_eq!(create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p), S_OK);
            let d = pipe(p);
            assert_eq!(
                d.stages,
                [Stage {
                    name: "compute",
                    bytes: cs.to_vec()
                }]
            );
            assert!(d.graphics.is_none());
        }
    }

    #[test]
    fn bad_streams() {
        unsafe {
            let mut p = std::ptr::dangling_mut();
            // Unknown subobject type.
            let s = [99u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let desc = [s.len(), s.as_ptr() as usize];
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            assert!(p.is_null());
            // A subobject that runs past the end.
            let s = [2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let desc = [s.len(), s.as_ptr() as usize];
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            // Null stream, zero size, no shader at all.
            let desc = [8usize, 0];
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            let s = [0u8; 16];
            let desc = [0usize, s.as_ptr() as usize];
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            let desc = [16usize, s.as_ptr() as usize]; // a root signature subobject only
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], &mut p),
                E_INVALIDARG
            );
            // No output pointer: S_FALSE.
            let code = [1u8];
            let mut s = vec![0u8; 24];
            put32(&mut s, 0, 6);
            put(&mut s, 8, code.as_ptr() as u64);
            put(&mut s, 16, 1);
            let desc = [s.len(), s.as_ptr() as usize];
            assert_eq!(
                create_stream(N, &desc, &PIPELINE_IIDS[0], std::ptr::null_mut()),
                S_FALSE
            );
        }
    }
}
