//! `mmdevapi` / audioclient / spatial audio: a null audio endpoint reachable through COM.
//!
//! The title creates `MMDeviceEnumerator`, takes the default render device and asks it for
//! `IAudioClient` and `ISpatialAudioClient`. Here one fake device exists; nothing is ever played.
//! Every vtable slot that is not implemented is a named trap (`unimplemented IMMDevice slot N
//! (+0xOFF)`, then abort), so the next missing call names itself.
//!
//! Objects share one layout ([`Obj`]: vtable pointer, count, kind) and one `QueryInterface`;
//! the kind decides the IIDs it answers to and its vtable. Slot numbers are the published
//! interface orders (IUnknown takes 0-2).
//!
//! Choices: the device reports `PKEY_AudioEndpoint_FormFactor` (`{1DA5D803-D492-4EDD-8C23-
//! E0C0FFEE7F0E}`, pid 0, as read from the title's key table) as `Speakers` (1, a VT_UI4); the
//! title compares it with 3 and 5 (headphones, headset) to pick a downmix. Other property keys
//! answer `VT_EMPTY` with `S_OK`. The mix format is float32, 48 kHz, stereo (mask 3).

use crate::com::{
    Guid, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER, HRESULT, IID_IUNKNOWN, S_OK,
};
use std::ffi::c_void;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xBCDE0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xA95664D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IMMDEVICE_COLLECTION: Guid = Guid::new(
    0x0BD7A1BE,
    0x7A1A,
    0x44DB,
    [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E],
);
const IID_IMMDEVICE: Guid = Guid::new(
    0xD666063F,
    0x1587,
    0x4E43,
    [0x81, 0xF1, 0xB9, 0x48, 0xE8, 0x07, 0x36, 0x3F],
);
const IID_IPROPERTY_STORE: Guid = Guid::new(
    0x886D8EEB,
    0x8CF2,
    0x4446,
    [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99],
);
pub const IID_IAUDIO_CLIENT: Guid = Guid::new(
    0x1CB9AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
pub const IID_ISPATIAL_AUDIO_CLIENT: Guid = Guid::new(
    0xBBF8E066,
    0xAAAA,
    0x49BE,
    [0x9A, 0x4D, 0xFD, 0x2A, 0x85, 0x8E, 0xA2, 0x7F],
);
const IID_IAUDIO_FORMAT_ENUMERATOR: Guid = Guid::new(
    0xDCDAA3E4,
    0xB8F6,
    0x4D3F,
    [0x97, 0x0C, 0x3A, 0x1F, 0x64, 0x8A, 0xA3, 0x50],
);
/// `PKEY_AudioEndpoint_FormFactor` = {1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E}, 0.
const PKEY_FORM_FACTOR: Guid = Guid::new(
    0x1DA5D803,
    0xD492,
    0x4EDD,
    [0x8C, 0x23, 0xE0, 0xC0, 0xFF, 0xEE, 0x7F, 0x0E],
);
const IID_ISPATIAL_RENDER_STREAM: Guid = Guid::new(
    0xBAB5F473,
    0xB423,
    0x477B,
    [0x85, 0xF5, 0xB5, 0xA3, 0x32, 0xA0, 0x41, 0x53],
);
const IID_ISPATIAL_RENDER_STREAM_BASE: Guid = Guid::new(
    0xFEAAF403,
    0xC1D8,
    0x450D,
    [0xAA, 0x05, 0xE0, 0xCC, 0xEE, 0x75, 0x02, 0xA8],
);
const IID_ISPATIAL_AUDIO_OBJECT: Guid = Guid::new(
    0xDDE28967,
    0x521B,
    0x46E5,
    [0x8F, 0x00, 0xBD, 0x68, 0x68, 0xB1, 0xA6, 0x45],
);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid::new(
    3,
    0,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

pub const E_NOAGGREGATION: HRESULT = 0x8004_0110u32 as i32;
pub const AUDCLNT_E_UNSUPPORTED_FORMAT: HRESULT = 0x8889_0008u32 as i32;
const DEVICE_STATE_ACTIVE: u32 = 1;
const ENDPOINT_FORM_FACTOR_SPEAKERS: u32 = 1;
const VT_UI4: u16 = 19;
const VT_BLOB: u16 = 65;
pub const SAMPLE_RATE: u32 = 48_000;

/// The mix format: WAVEFORMATEXTENSIBLE, float32, 48 kHz, stereo.
const MIX_FORMAT_SIZE: usize = 40;
fn mix_format_bytes() -> [u8; MIX_FORMAT_SIZE] {
    let mut b = [0u8; MIX_FORMAT_SIZE];
    b[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
    b[2..4].copy_from_slice(&2u16.to_le_bytes());
    b[4..8].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    b[8..12].copy_from_slice(&(SAMPLE_RATE * 8).to_le_bytes());
    b[12..14].copy_from_slice(&8u16.to_le_bytes());
    b[14..16].copy_from_slice(&32u16.to_le_bytes());
    b[16..18].copy_from_slice(&22u16.to_le_bytes());
    b[18..20].copy_from_slice(&32u16.to_le_bytes());
    b[20..24].copy_from_slice(&3u32.to_le_bytes());
    let g = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    b[24..28].copy_from_slice(&g.data1.to_le_bytes());
    b[28..30].copy_from_slice(&g.data2.to_le_bytes());
    b[30..32].copy_from_slice(&g.data3.to_le_bytes());
    b[32..40].copy_from_slice(&g.data4);
    b
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
enum Kind {
    Enumerator,
    Collection,
    Device,
    PropertyStore,
    AudioClient,
    SpatialClient,
    FormatEnumerator,
    Stream,
    SpatialObject,
}

const NAMES: [&str; 9] = [
    "IMMDeviceEnumerator",
    "IMMDeviceCollection",
    "IMMDevice",
    "IPropertyStore",
    "IAudioClient",
    "ISpatialAudioClient",
    "IAudioFormatEnumerator",
    "ISpatialAudioObjectRenderStream",
    "ISpatialAudioObject",
];

impl Kind {
    fn answers(self, iid: &Guid) -> bool {
        *iid == self.iid() || (self == Kind::Stream && *iid == IID_ISPATIAL_RENDER_STREAM_BASE)
    }
    fn iid(self) -> Guid {
        match self {
            Kind::Enumerator => IID_IMMDEVICE_ENUMERATOR,
            Kind::Collection => IID_IMMDEVICE_COLLECTION,
            Kind::Device => IID_IMMDEVICE,
            Kind::PropertyStore => IID_IPROPERTY_STORE,
            Kind::AudioClient => IID_IAUDIO_CLIENT,
            Kind::SpatialClient => IID_ISPATIAL_AUDIO_CLIENT,
            Kind::FormatEnumerator => IID_IAUDIO_FORMAT_ENUMERATOR,
            Kind::Stream => IID_ISPATIAL_RENDER_STREAM,
            Kind::SpatialObject => IID_ISPATIAL_AUDIO_OBJECT,
        }
    }
}

/// Every object: vtable pointer first (what the title sees), then count and kind.
#[repr(C)]
struct Obj {
    vtbl: &'static Table,
    refs: AtomicU32,
    kind: Kind,
    extra: Extra,
}

const SLOTS: usize = 32;
#[repr(C)]
struct Table([*const (); SLOTS]);
// SAFETY: immutable code addresses.
unsafe impl Sync for Table {}

extern "win64" fn trap<const IFACE: usize, const SLOT: usize>() -> ! {
    eprintln!(
        "nativ64: unimplemented {} slot {} (+{:#x})",
        NAMES[IFACE],
        SLOT,
        SLOT * 8
    );
    // SAFETY: abort never returns.
    unsafe { libc::abort() }
}

macro_rules! row {
    ($i:literal, $b:literal) => {
        [
            trap::<$i, { $b }> as *const (),
            trap::<$i, { $b + 1 }> as *const (),
            trap::<$i, { $b + 2 }> as *const (),
            trap::<$i, { $b + 3 }> as *const (),
            trap::<$i, { $b + 4 }> as *const (),
            trap::<$i, { $b + 5 }> as *const (),
            trap::<$i, { $b + 6 }> as *const (),
            trap::<$i, { $b + 7 }> as *const (),
            trap::<$i, { $b + 8 }> as *const (),
            trap::<$i, { $b + 9 }> as *const (),
            trap::<$i, { $b + 10 }> as *const (),
            trap::<$i, { $b + 11 }> as *const (),
            trap::<$i, { $b + 12 }> as *const (),
            trap::<$i, { $b + 13 }> as *const (),
            trap::<$i, { $b + 14 }> as *const (),
            trap::<$i, { $b + 15 }> as *const (),
        ]
    };
}

/// A vtable of traps for interface `$i`, with `slot => function` overrides.
macro_rules! vtable {
    ($i:literal; $($slot:literal => $f:expr),* $(,)?) => {{
        let r0 = row!($i, 0);
        let r1 = row!($i, 16);
        let mut t = [std::ptr::null::<()>(); SLOTS];
        let mut k = 0;
        while k < 16 {
            t[k] = r0[k];
            t[16 + k] = r1[k];
            k += 1;
        }
        t[0] = query_interface as *const ();
        t[1] = add_ref as *const ();
        t[2] = release as *const ();
        $(t[$slot] = $f as *const ();)*
        Table(t)
    }};
}

type Out = *mut *mut c_void;

/// State a stream shares with its clock thread.
struct StreamShared {
    started: AtomicBool,
    stop: AtomicBool,
    event: usize,
    max_objects: u32,
}

/// Per-object payload.
enum Extra {
    None,
    Stream {
        shared: Arc<StreamShared>,
        clock_running: bool,
    },
    /// An audio object's buffer (mono float32, one stream period); its bytes are never read.
    Buffer(Box<[f32]>),
}

impl Drop for Obj {
    fn drop(&mut self) {
        if let Extra::Stream { shared, .. } = &self.extra {
            shared.stop.store(true, Ordering::Release);
        }
    }
}

/// Frames per stream period: 10 ms at 48 kHz.
const PERIOD_FRAMES: u32 = SAMPLE_RATE / 100;

unsafe fn new_obj(kind: Kind) -> *mut c_void {
    new_obj_with(kind, Extra::None)
}

unsafe fn new_obj_with(kind: Kind, extra: Extra) -> *mut c_void {
    let vtbl: &'static Table = match kind {
        Kind::Enumerator => &ENUMERATOR,
        Kind::Collection => &COLLECTION,
        Kind::Device => &DEVICE,
        Kind::PropertyStore => &PROPERTY_STORE,
        Kind::AudioClient => &AUDIO_CLIENT,
        Kind::SpatialClient => &SPATIAL_CLIENT,
        Kind::FormatEnumerator => &FORMAT_ENUMERATOR,
        Kind::Stream => &STREAM,
        Kind::SpatialObject => &SPATIAL_OBJECT,
    };
    Box::into_raw(Box::new(Obj {
        vtbl,
        refs: AtomicU32::new(1),
        kind,
        extra,
    }))
    .cast()
}

/// Stores a new object of `kind` in `*out` (non-NULL).
unsafe fn give(out: Out, kind: Kind) -> HRESULT {
    *out = new_obj(kind);
    S_OK
}

unsafe extern "win64" fn query_interface(this: *mut Obj, iid: *const Guid, out: Out) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if iid.is_null() {
        return E_POINTER;
    }
    let iid = *iid;
    if iid == IID_IUNKNOWN || (*this).kind.answers(&iid) {
        add_ref(this);
        *out = this.cast();
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn add_ref(this: *mut Obj) -> u32 {
    (*this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn release(this: *mut Obj) -> u32 {
    let left = (*this).refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        fence(Ordering::Acquire);
        drop(Box::from_raw(this));
    }
    left
}

/// A CoTaskMem copy of `bytes`.
unsafe fn task_mem(bytes: &[u8]) -> *mut u8 {
    let p: *mut u8 = libc::malloc(bytes.len().max(1)).cast();
    if !p.is_null() {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
    p
}

// ---- IMMDeviceEnumerator ----

/// `EnumAudioEndpoints(dataflow, state mask, &collection)`: one device when the mask has ACTIVE.
unsafe extern "win64" fn enum_endpoints(
    _this: *mut Obj,
    flow: u32,
    mask: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if flow > 2 || mask == 0 || mask & !0xF != 0 {
        return E_INVALIDARG;
    }
    give(out, Kind::Collection)
}

/// `GetDefaultAudioEndpoint(dataflow, role, &device)`: render and capture flows only.
unsafe extern "win64" fn default_endpoint(
    _this: *mut Obj,
    flow: u32,
    role: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if flow >= 2 || role >= 3 {
        return E_INVALIDARG;
    }
    give(out, Kind::Device)
}

unsafe extern "win64" fn register_notification(_this: *mut Obj, client: *mut c_void) -> HRESULT {
    // Shortcut: the callback is never stored, no endpoint ever changes.
    if client.is_null() {
        E_POINTER
    } else {
        S_OK
    }
}

// ---- IMMDeviceCollection ----

unsafe extern "win64" fn collection_count(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "win64" fn collection_item(_this: *mut Obj, index: u32, out: Out) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if index != 0 {
        return E_INVALIDARG;
    }
    give(out, Kind::Device)
}

// ---- IMMDevice ----

/// `Activate(iid, clsctx, params, &interface)`.
unsafe extern "win64" fn device_activate(
    _this: *mut Obj,
    iid: *const Guid,
    _clsctx: u32,
    _params: *mut c_void,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if iid.is_null() {
        return E_POINTER;
    }
    let iid = *iid;
    if iid == IID_IAUDIO_CLIENT {
        give(out, Kind::AudioClient)
    } else if iid == IID_ISPATIAL_AUDIO_CLIENT {
        give(out, Kind::SpatialClient)
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn device_open_property_store(
    _this: *mut Obj,
    _access: u32,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    give(out, Kind::PropertyStore)
}

const DEVICE_ID: &str = "{0.0.0.00000000}.{00000000-0000-0000-0000-000000000001}";

unsafe extern "win64" fn device_get_id(_this: *mut Obj, out: *mut *mut u16) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let mut bytes = Vec::new();
    for u in DEVICE_ID.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let p = task_mem(&bytes);
    *out = p.cast();
    if p.is_null() {
        E_FAIL
    } else {
        S_OK
    }
}

unsafe extern "win64" fn device_get_state(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = DEVICE_STATE_ACTIVE;
    S_OK
}

// ---- IPropertyStore ----

/// `GetValue(&PROPERTYKEY, &PROPVARIANT)`: only the form factor has a value.
unsafe extern "win64" fn store_get_value(
    _this: *mut Obj,
    key: *const [u8; 20],
    value: *mut u8,
) -> HRESULT {
    if key.is_null() || value.is_null() {
        return E_POINTER;
    }
    std::ptr::write_bytes(value, 0, 24);
    let key = *key;
    let g = PKEY_FORM_FACTOR;
    let mut want = [0u8; 20];
    want[0..4].copy_from_slice(&g.data1.to_le_bytes());
    want[4..6].copy_from_slice(&g.data2.to_le_bytes());
    want[6..8].copy_from_slice(&g.data3.to_le_bytes());
    want[8..16].copy_from_slice(&g.data4);
    if key == want {
        value.cast::<u16>().write_unaligned(VT_UI4);
        value
            .add(8)
            .cast::<u32>()
            .write_unaligned(ENDPOINT_FORM_FACTOR_SPEAKERS);
    }
    S_OK
}

// ---- IAudioClient ----

unsafe extern "win64" fn client_get_mix_format(_this: *mut Obj, out: *mut *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let p = task_mem(&mix_format_bytes());
    *out = p;
    if p.is_null() {
        E_FAIL
    } else {
        S_OK
    }
}

// ---- ISpatialAudioClient ----

/// Float, 48 kHz, mono or stereo-or-more channels as WAVEFORMATEX(EXTENSIBLE).
unsafe fn format_supported(fmt: *const u8) -> bool {
    let tag = fmt.cast::<u16>().read_unaligned();
    let rate = fmt.add(4).cast::<u32>().read_unaligned();
    let bits = fmt.add(14).cast::<u16>().read_unaligned();
    let float = match tag {
        3 => true,
        0xFFFE => {
            let sub = fmt.add(24).cast::<u32>().read_unaligned();
            sub == 3
        }
        _ => false,
    };
    float && bits == 32 && rate == SAMPLE_RATE
}

/// Dynamic objects a stream may ask for. Windows Sonic offers 15 plus the static ones; the number
/// only has to be positive for a title that checks it.
const MAX_DYNAMIC_OBJECTS: u32 = 16;
/// Static speaker mask: front left and right, as the mix format.
const STATIC_MASK: u32 = 3;

/// `GetStaticObjectPosition(type, &x, &y, &z)`: unit-sphere guesses per speaker bit (unverified;
/// x right, y up, z towards the listener, so front is -1).
unsafe extern "win64" fn spatial_static_position(
    _this: *mut Obj,
    ty: u32,
    x: *mut f32,
    y: *mut f32,
    z: *mut f32,
) -> HRESULT {
    if x.is_null() || y.is_null() || z.is_null() {
        return E_POINTER;
    }
    if ty.count_ones() != 1 || ty & 0x3_FFFF == 0 {
        return E_INVALIDARG;
    }
    let (px, py, pz) = match ty {
        0x1 => (-1.0, 0.0, -1.0),
        0x2 => (1.0, 0.0, -1.0),
        0x4 => (0.0, 0.0, -1.0),
        0x8 => (0.0, -1.0, 0.0),
        0x10 => (-1.0, 0.0, 1.0),
        0x20 => (1.0, 0.0, 1.0),
        _ => (0.0, 0.0, 0.0),
    };
    *x = px;
    *y = py;
    *z = pz;
    S_OK
}

unsafe extern "win64" fn spatial_static_mask(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = STATIC_MASK;
    S_OK
}

unsafe extern "win64" fn spatial_max_dynamic(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = MAX_DYNAMIC_OBJECTS;
    S_OK
}

/// `GetMaxFrameCount(format, &frames)`: one 10 ms period.
unsafe extern "win64" fn spatial_max_frames(
    _this: *mut Obj,
    fmt: *const u8,
    out: *mut u32,
) -> HRESULT {
    if fmt.is_null() || out.is_null() {
        return E_POINTER;
    }
    if !format_supported(fmt) {
        return AUDCLNT_E_UNSUPPORTED_FORMAT;
    }
    *out = PERIOD_FRAMES;
    S_OK
}

unsafe extern "win64" fn spatial_stream_available(
    _this: *mut Obj,
    iid: *const Guid,
    _params: *const u8,
) -> HRESULT {
    if iid.is_null() {
        return E_POINTER;
    }
    S_OK
}

unsafe extern "win64" fn spatial_get_format_enumerator(_this: *mut Obj, out: Out) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    give(out, Kind::FormatEnumerator)
}

unsafe extern "win64" fn spatial_is_format_supported(_this: *mut Obj, fmt: *const u8) -> HRESULT {
    if fmt.is_null() {
        return E_POINTER;
    }
    if format_supported(fmt) {
        S_OK
    } else {
        AUDCLNT_E_UNSUPPORTED_FORMAT
    }
}

/// `ActivateSpatialAudioStream(&PROPVARIANT params, iid, &stream)`. The PROPVARIANT is a VT_BLOB
/// holding `SpatialAudioObjectRenderStreamActivationParams`: format pointer, static type mask,
/// min / max dynamic objects (+12, +16), category, event handle (+24), notify object.
unsafe extern "win64" fn spatial_activate_stream(
    _this: *mut Obj,
    params: *const u8,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if params.is_null() || iid.is_null() {
        return E_POINTER;
    }
    let iid = *iid;
    if iid != IID_ISPATIAL_RENDER_STREAM && iid != IID_ISPATIAL_RENDER_STREAM_BASE {
        return E_NOINTERFACE;
    }
    let vt = params.cast::<u16>().read_unaligned();
    let blob = params.add(16).cast::<*const u8>().read_unaligned();
    if vt != VT_BLOB || blob.is_null() {
        return E_INVALIDARG;
    }
    let max_objects = blob.add(16).cast::<u32>().read_unaligned();
    let event = blob.add(24).cast::<usize>().read_unaligned();
    let shared = Arc::new(StreamShared {
        started: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        event,
        max_objects,
    });
    *out = new_obj_with(
        Kind::Stream,
        Extra::Stream {
            shared,
            clock_running: false,
        },
    );
    S_OK
}

// ---- ISpatialAudioObjectRenderStream (null device: a clock thread signals the event) ----

unsafe fn stream_shared<'a>(this: *mut Obj) -> &'a Arc<StreamShared> {
    match &(*this).extra {
        Extra::Stream { shared, .. } => shared,
        _ => unreachable!("not a stream"),
    }
}

unsafe extern "win64" fn stream_available_objects(this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = stream_shared(this).max_objects;
    S_OK
}

unsafe extern "win64" fn stream_start(this: *mut Obj) -> HRESULT {
    let shared = stream_shared(this).clone();
    shared.started.store(true, Ordering::Release);
    if let Extra::Stream { clock_running, .. } = &mut (*this).extra {
        if !*clock_running {
            *clock_running = true;
            std::thread::spawn(move || {
                while !shared.stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(10));
                    if shared.started.load(Ordering::Acquire) && shared.event != 0 {
                        crate::handles::set_event(shared.event);
                    }
                }
            });
        }
    }
    S_OK
}

unsafe extern "win64" fn stream_stop(this: *mut Obj) -> HRESULT {
    stream_shared(this).started.store(false, Ordering::Release);
    S_OK
}

unsafe extern "win64" fn stream_reset(_this: *mut Obj) -> HRESULT {
    S_OK
}

/// `BeginUpdatingAudioObjects(&available, &frames)`.
unsafe extern "win64" fn stream_begin(
    this: *mut Obj,
    avail: *mut u32,
    frames: *mut u32,
) -> HRESULT {
    if avail.is_null() || frames.is_null() {
        return E_POINTER;
    }
    *avail = stream_shared(this).max_objects;
    *frames = PERIOD_FRAMES;
    S_OK
}

unsafe extern "win64" fn stream_end(_this: *mut Obj) -> HRESULT {
    S_OK
}

/// `ActivateSpatialAudioObject(type, &object)`.
unsafe extern "win64" fn stream_activate_object(_this: *mut Obj, _ty: u32, out: Out) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = new_obj_with(
        Kind::SpatialObject,
        Extra::Buffer(vec![0f32; PERIOD_FRAMES as usize].into_boxed_slice()),
    );
    S_OK
}

// ---- ISpatialAudioObject ----

/// `GetBuffer(&bytes_ptr, &byte_count)`: a scratch period that is discarded.
unsafe extern "win64" fn object_get_buffer(
    this: *mut Obj,
    buf: *mut *mut u8,
    bytes: *mut u32,
) -> HRESULT {
    if buf.is_null() || bytes.is_null() {
        return E_POINTER;
    }
    match &mut (*this).extra {
        Extra::Buffer(b) => {
            *buf = b.as_mut_ptr().cast();
            *bytes = (b.len() * 4) as u32;
            S_OK
        }
        _ => E_FAIL,
    }
}

unsafe extern "win64" fn object_set_end_of_stream(_this: *mut Obj, _frames: u32) -> HRESULT {
    S_OK
}

unsafe extern "win64" fn object_is_active(_this: *mut Obj, out: *mut i32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "win64" fn object_get_type(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 0; // AudioObjectType_Dynamic
    S_OK
}

unsafe extern "win64" fn object_set_position(
    _this: *mut Obj,
    _x: f32,
    _y: f32,
    _z: f32,
) -> HRESULT {
    S_OK
}

unsafe extern "win64" fn object_set_volume(_this: *mut Obj, _volume: f32) -> HRESULT {
    S_OK
}

// ---- IAudioFormatEnumerator ----

unsafe extern "win64" fn formats_count(_this: *mut Obj, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

/// The enumerator's own copy of the format; the title only reads it.
static MIX_FORMAT_STATIC: std::sync::OnceLock<[u8; MIX_FORMAT_SIZE]> = std::sync::OnceLock::new();

unsafe extern "win64" fn formats_get(_this: *mut Obj, index: u32, out: *mut *const u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null();
    if index != 0 {
        return E_INVALIDARG;
    }
    *out = MIX_FORMAT_STATIC.get_or_init(mix_format_bytes).as_ptr();
    S_OK
}

static ENUMERATOR: Table = vtable!(0;
    3 => enum_endpoints, 4 => default_endpoint,
    6 => register_notification, 7 => register_notification);
static COLLECTION: Table = vtable!(1; 3 => collection_count, 4 => collection_item);
static DEVICE: Table = vtable!(2;
    3 => device_activate, 4 => device_open_property_store,
    5 => device_get_id, 6 => device_get_state);
static PROPERTY_STORE: Table = vtable!(3; 5 => store_get_value);
static AUDIO_CLIENT: Table = vtable!(4; 8 => client_get_mix_format);
static SPATIAL_CLIENT: Table = vtable!(5;
    3 => spatial_static_position, 4 => spatial_static_mask, 5 => spatial_max_dynamic,
    6 => spatial_get_format_enumerator, 7 => spatial_max_frames,
    8 => spatial_is_format_supported, 9 => spatial_stream_available,
    10 => spatial_activate_stream);
static STREAM: Table = vtable!(7;
    3 => stream_available_objects, 5 => stream_start, 6 => stream_stop, 7 => stream_reset,
    8 => stream_begin, 9 => stream_end, 10 => stream_activate_object);
static SPATIAL_OBJECT: Table = vtable!(8;
    3 => object_get_buffer, 4 => object_set_end_of_stream, 5 => object_is_active,
    6 => object_get_type, 7 => object_set_position, 8 => object_set_volume);
static FORMAT_ENUMERATOR: Table = vtable!(6; 3 => formats_count, 4 => formats_get);

/// `CoCreateInstance` for `MMDeviceEnumerator`: `Ok` with the object (already queried for
/// `iid`), or the failing HRESULT with `*out` NULL.
///
/// # Safety
/// `iid` and `out` are valid, non-NULL pointers.
pub unsafe fn create_enumerator(outer: *mut u8, iid: *const Guid, out: Out) -> HRESULT {
    *out = std::ptr::null_mut();
    if !outer.is_null() {
        return E_NOAGGREGATION;
    }
    if iid.is_null() {
        return E_POINTER;
    }
    let obj = new_obj(Kind::Enumerator).cast::<Obj>();
    let hr = query_interface(obj, iid, out);
    release(obj);
    hr
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{null, null_mut};

    unsafe fn slot<F: Copy>(o: *mut c_void, n: usize) -> F {
        let vt = *(o as *const *const *const ());
        std::mem::transmute_copy(&*vt.add(n))
    }
    unsafe fn rel(o: *mut c_void) {
        let f: unsafe extern "win64" fn(*mut c_void) -> u32 = slot(o, 2);
        f(o);
    }

    unsafe fn enumerator() -> *mut c_void {
        let mut e = null_mut();
        assert_eq!(
            create_enumerator(null_mut(), &IID_IMMDEVICE_ENUMERATOR, &mut e),
            S_OK
        );
        e
    }

    unsafe fn default_device(e: *mut c_void) -> *mut c_void {
        let f: unsafe extern "win64" fn(*mut c_void, u32, u32, Out) -> HRESULT = slot(e, 4);
        let mut d = null_mut();
        assert_eq!(f(e, 0, 0, &mut d), S_OK);
        d
    }

    unsafe fn activate(d: *mut c_void, iid: &Guid, out: Out) -> HRESULT {
        let f: unsafe extern "win64" fn(
            *mut c_void,
            *const Guid,
            u32,
            *mut c_void,
            Out,
        ) -> HRESULT = slot(d, 3);
        f(d, iid, 1, null_mut(), out)
    }

    #[test]
    fn enumerator_to_mix_format() {
        unsafe {
            let e = enumerator();
            let d = default_device(e);
            let mut c = null_mut();
            assert_eq!(activate(d, &IID_IAUDIO_CLIENT, &mut c), S_OK);
            let f: unsafe extern "win64" fn(*mut c_void, *mut *mut u8) -> HRESULT = slot(c, 8);
            let mut fmt = null_mut();
            assert_eq!(f(c, &mut fmt), S_OK);
            assert!(format_supported(fmt));
            assert_eq!(fmt.add(2).cast::<u16>().read_unaligned(), 2);
            libc::free(fmt.cast());
            rel(c);
            rel(d);
            rel(e);
        }
    }

    #[test]
    fn collection_has_one_device() {
        unsafe {
            let e = enumerator();
            let f: unsafe extern "win64" fn(*mut c_void, u32, u32, Out) -> HRESULT = slot(e, 3);
            let mut coll = null_mut();
            assert_eq!(f(e, 0, DEVICE_STATE_ACTIVE, &mut coll), S_OK);
            let count: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = slot(coll, 3);
            let mut n = 0;
            assert_eq!(count(coll, &mut n), S_OK);
            assert_eq!(n, 1);
            let item: unsafe extern "win64" fn(*mut c_void, u32, Out) -> HRESULT = slot(coll, 4);
            let mut d = null_mut();
            assert_eq!(item(coll, 1, &mut d), E_INVALIDARG);
            assert!(d.is_null());
            assert_eq!(item(coll, 0, &mut d), S_OK);
            let state: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = slot(d, 6);
            let mut s = 0;
            assert_eq!(state(d, &mut s), S_OK);
            assert_eq!(s, DEVICE_STATE_ACTIVE);
            rel(d);
            rel(coll);
            rel(e);
        }
    }

    #[test]
    fn unknown_iid_and_null_outs() {
        unsafe {
            let e = enumerator();
            let d = default_device(e);
            let other = Guid::new(9, 9, 9, [9; 8]);
            let mut out = 8 as *mut c_void;
            assert_eq!(activate(d, &other, &mut out), E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(activate(d, &IID_IAUDIO_CLIENT, null_mut()), E_POINTER);
            let qi: unsafe extern "win64" fn(*mut c_void, *const Guid, Out) -> HRESULT = slot(d, 0);
            out = 8 as *mut c_void;
            assert_eq!(qi(d, &other, &mut out), E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(qi(d, &IID_IMMDEVICE, null_mut()), E_POINTER);
            let def: unsafe extern "win64" fn(*mut c_void, u32, u32, Out) -> HRESULT = slot(e, 4);
            assert_eq!(def(e, 0, 0, null_mut()), E_POINTER);
            assert_eq!(
                create_enumerator(null_mut(), &other, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            let mut id = null_mut::<u16>();
            let get_id: unsafe extern "win64" fn(*mut c_void, *mut *mut u16) -> HRESULT =
                slot(d, 5);
            assert_eq!(get_id(d, null_mut()), E_POINTER);
            assert_eq!(get_id(d, &mut id), S_OK);
            assert_eq!(*id, b'{' as u16);
            libc::free(id.cast());
            let _ = null::<u8>();
            rel(d);
            rel(e);
        }
    }

    #[test]
    fn form_factor_is_speakers_and_spatial_formats() {
        unsafe {
            let e = enumerator();
            let d = default_device(e);
            let open: unsafe extern "win64" fn(*mut c_void, u32, Out) -> HRESULT = slot(d, 4);
            let mut ps = null_mut();
            assert_eq!(open(d, 0, &mut ps), S_OK);
            let get: unsafe extern "win64" fn(*mut c_void, *const [u8; 20], *mut u8) -> HRESULT =
                slot(ps, 5);
            let g = PKEY_FORM_FACTOR;
            let mut key = [0u8; 20];
            key[0..4].copy_from_slice(&g.data1.to_le_bytes());
            key[4..6].copy_from_slice(&g.data2.to_le_bytes());
            key[6..8].copy_from_slice(&g.data3.to_le_bytes());
            key[8..16].copy_from_slice(&g.data4);
            let mut pv = [0xFFu8; 24];
            assert_eq!(get(ps, &key, pv.as_mut_ptr()), S_OK);
            assert_eq!(u16::from_le_bytes([pv[0], pv[1]]), VT_UI4);
            assert_eq!(pv[8], 1);
            key[16] = 7;
            assert_eq!(get(ps, &key, pv.as_mut_ptr()), S_OK);
            assert_eq!(u16::from_le_bytes([pv[0], pv[1]]), 0);
            rel(ps);

            let mut sc = null_mut();
            assert_eq!(activate(d, &IID_ISPATIAL_AUDIO_CLIENT, &mut sc), S_OK);
            let en: unsafe extern "win64" fn(*mut c_void, Out) -> HRESULT = slot(sc, 6);
            let mut fe = null_mut();
            assert_eq!(en(sc, &mut fe), S_OK);
            let count: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = slot(fe, 3);
            let mut n = 0;
            assert_eq!(count(fe, &mut n), S_OK);
            assert!(n >= 1);
            let getf: unsafe extern "win64" fn(*mut c_void, u32, *mut *const u8) -> HRESULT =
                slot(fe, 4);
            let mut f = null();
            assert_eq!(getf(fe, 0, &mut f), S_OK);
            let sup: unsafe extern "win64" fn(*mut c_void, *const u8) -> HRESULT = slot(sc, 8);
            assert_eq!(sup(sc, f), S_OK);
            let mut bad = mix_format_bytes();
            bad[4] = 0;
            assert_eq!(sup(sc, bad.as_ptr()), AUDCLNT_E_UNSUPPORTED_FORMAT);
            rel(fe);
            rel(sc);
            rel(d);
            rel(e);
        }
    }

    #[test]
    fn stream_clock_signals_the_event() {
        unsafe {
            let e = enumerator();
            let d = default_device(e);
            let mut sc = null_mut();
            assert_eq!(activate(d, &IID_ISPATIAL_AUDIO_CLIENT, &mut sc), S_OK);
            let ev = crate::handles::create_event(None, false, false);
            let mut blob = [0u8; 48];
            blob[16..20].copy_from_slice(&4u32.to_le_bytes());
            blob[24..32].copy_from_slice(&ev.to_le_bytes());
            let mut pv = [0u8; 24];
            pv[0..2].copy_from_slice(&VT_BLOB.to_le_bytes());
            pv[8..12].copy_from_slice(&48u32.to_le_bytes());
            pv[16..24].copy_from_slice(&(blob.as_ptr() as usize).to_le_bytes());
            let act: unsafe extern "win64" fn(*mut c_void, *const u8, *const Guid, Out) -> HRESULT =
                slot(sc, 10);
            let mut st = null_mut();
            let other = Guid::new(9, 9, 9, [9; 8]);
            assert_eq!(act(sc, pv.as_ptr(), &other, &mut st), E_NOINTERFACE);
            assert!(st.is_null());
            assert_eq!(
                act(sc, pv.as_ptr(), &IID_ISPATIAL_RENDER_STREAM, &mut st),
                S_OK
            );
            let start: unsafe extern "win64" fn(*mut c_void) -> HRESULT = slot(st, 5);
            assert_eq!(crate::handles::wait(&[ev], false, 0), 0x102);
            assert_eq!(start(st), S_OK);
            assert_eq!(crate::handles::wait(&[ev], false, 500), 0);
            let begin: unsafe extern "win64" fn(*mut c_void, *mut u32, *mut u32) -> HRESULT =
                slot(st, 8);
            let (mut n, mut frames) = (0, 0);
            assert_eq!(begin(st, &mut n, &mut frames), S_OK);
            assert_eq!((n, frames), (4, 480));
            let mk: unsafe extern "win64" fn(*mut c_void, u32, Out) -> HRESULT = slot(st, 10);
            let mut obj = null_mut();
            assert_eq!(mk(st, 0, &mut obj), S_OK);
            let getb: unsafe extern "win64" fn(*mut c_void, *mut *mut u8, *mut u32) -> HRESULT =
                slot(obj, 3);
            let (mut p, mut bytes) = (null_mut(), 0);
            assert_eq!(getb(obj, &mut p, &mut bytes), S_OK);
            assert_eq!(bytes, 480 * 4);
            rel(obj);
            rel(st);
            rel(sc);
            rel(d);
            rel(e);
        }
    }
}
