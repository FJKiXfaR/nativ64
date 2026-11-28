//! Descriptor heaps (`ID3D12DescriptorHeap`). A heap is title-visible memory of raw descriptors:
//! the heap start is the address of the first one and the handles are addresses, so the title
//! (or its inlined code) writes and copies descriptors by itself. Decoding them is the
//! backend's job, at replay.
//!
//! Newer titles follow the plain `D3D12_DESCRIPTOR_HEAP_DESC` with memory of their own: an
//! address and a size in bytes (a NULL address asks the heap to allocate). The plain and the
//! extended description share the slot, so the extension is recognised by its size matching
//! the descriptor count (shortcut; find the flag that tells them apart if a title trips on it).

use std::alloc::Layout;
use std::ffi::c_void;

use crate::d3d12::device::E_INVALIDARG;
use crate::d3d12::info::get_descriptor_increment_size;
use crate::d3d12::object::{guid, hand_out, with_child_methods, PAGEABLE_IIDS};
use crate::xobj::{Guid, Iface, SlotTable, XObject, HRESULT};

pub static DESCRIPTOR_HEAP_IIDS: [Guid; 4] = [
    guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// `D3D12_DESCRIPTOR_HEAP_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapDesc {
    /// 0 views, 1 samplers, 2 render targets, 3 depth targets.
    pub kind: u32,
    pub count: u32,
    pub flags: u32,
    pub node_mask: u32,
}

/// What newer titles pass after the plain description.
#[repr(C)]
#[derive(Clone, Copy)]
struct Placement {
    address: u64,
    size: u64,
}

/// Zeroed memory the heap owns, page aligned and a whole number of pages (the GPU backend
/// imports it as it imports title memory), freed on drop. Listed in [`heap_ranges`].
struct Memory {
    at: *mut u8,
    layout: Layout,
}

/// Page size the heaps' own memory is aligned and rounded to.
const HEAP_PAGE: usize = 4096;

/// `(address, length)` of the memory of every live heap that allocated its own.
static HEAP_RANGES: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// The memory of the live descriptor heaps that own theirs (heaps placed in title memory are
/// in the title's memory table, not here): what a shader may read that is not title memory.
pub fn heap_ranges() -> Vec<(usize, usize)> {
    HEAP_RANGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

impl Memory {
    fn zeroed(size: usize) -> Option<Memory> {
        let layout =
            Layout::from_size_align(size.max(1).next_multiple_of(HEAP_PAGE), HEAP_PAGE).ok()?;
        // SAFETY: a non-zero size.
        let at = unsafe { std::alloc::alloc_zeroed(layout) };
        if at.is_null() {
            return None;
        }
        HEAP_RANGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((at as usize, layout.size()));
        Some(Memory { at, layout })
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        let range = (self.at as usize, self.layout.size());
        HEAP_RANGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|r| *r != range);
        nativ64::api_ms_win_core_memory::note_change(range.0, range.1);
        // SAFETY: allocated with this layout.
        unsafe { std::alloc::dealloc(self.at, self.layout) };
    }
}

/// Console fields from `+0x08`; then ours.
#[repr(C)]
pub struct DescriptorHeapData {
    bits: u32,
    console_refs: u32,
    pub desc: HeapDesc,
    /// The first descriptor: in `memory`, or in the title's own memory.
    pub start: u64,
    memory: Option<Memory>,
}

// SAFETY: the raw pointer is memory this object owns.
unsafe impl Send for DescriptorHeapData {}

static TABLE: SlotTable<24> =
    with_child_methods(SlotTable::new(Iface::DescriptorHeap).with_iunknown::<DescriptorHeapData>())
        .with(8, get_desc as *const ())
        .with(9, get_start as *const ())
        .with(10, get_start as *const ());

/// `ID3D12Device::CreateDescriptorHeap(desc, riid, out)` (device slot 14).
///
/// # Safety
/// `desc` is NULL or a description (a placement may follow it); `iid` and `out` are NULL or valid.
pub unsafe extern "win64" fn create_descriptor_heap(
    _this: *mut c_void,
    desc: *const HeapDesc,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    let Some(&plain) = desc.as_ref() else {
        return E_INVALIDARG;
    };
    let step = get_descriptor_increment_size(std::ptr::null_mut(), plain.kind) as u64;
    if step == 0 {
        return E_INVALIDARG;
    }
    let size = step * u64::from(plain.count.max(1));
    // Read only as a hint: a title passing the plain description has its own data after it.
    let placement = desc.add(1).cast::<Placement>().read_unaligned();
    let placed = placement.address != 0 && placement.size == size;
    let (start, memory) = if placed {
        if placement.address.checked_add(size).is_none() {
            return E_INVALIDARG;
        }
        (placement.address, None)
    } else {
        let Some(m) = Memory::zeroed(size as usize) else {
            return crate::d3d12::object::E_OUTOFMEMORY;
        };
        (m.at as u64, Some(m))
    };
    let mut memory = memory;
    hand_out(
        &TABLE,
        Iface::DescriptorHeap,
        &DESCRIPTOR_HEAP_IIDS,
        move || DescriptorHeapData {
            bits: 0,
            console_refs: 0,
            desc: plain,
            start,
            memory: memory.take(),
        },
        iid,
        out,
    )
}

/// Slot 8, `GetDesc()`: a 16-byte struct, so the return pointer comes first and `this` second.
unsafe extern "win64" fn get_desc(out: *mut HeapDesc, this: *mut c_void) -> *mut HeapDesc {
    if !out.is_null() {
        out.write(XObject::<DescriptorHeapData>::data(this).desc);
    }
    out
}

/// Slots 9 and 10, `GetCPUDescriptorHandleForHeapStart()` and the GPU one: the same address,
/// returned in `rax` (an 8-byte struct; to confirm at a call site).
unsafe extern "win64" fn get_start(this: *mut c_void) -> u64 {
    XObject::<DescriptorHeapData>::data(this).start
}

/// What the title described at a descriptor (descriptions kept as the words it passed, for the
/// backend to build its own views from at replay).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// Constant buffer: GPU address and size.
    Constant(u64, u32),
    /// Shader resource: resource, description (none: the resource's own).
    Shader(usize, Option<[u32; 10]>),
    /// Unordered access: resource, counter resource, description.
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    /// The console's sampler description: filter, addressing, LOD, anisotropy and border words,
    /// then console-only tuning words (27 in all).
    ConsoleSampler([u32; 27]),
    /// A view of memory without a resource object (device slots 70 and 71): whether it is
    /// unordered access, the resource description (as words), then the view's (the plain one
    /// followed by the memory address and console fields).
    Placed(bool, [u32; 14], [u32; 16]),
}

/// Tag of a view or sampler descriptor: ours, in the descriptor's first word; the second word
/// indexes [`VIEWS`]. The rest of the slot stays as it was, so a title that copies descriptors
/// as bytes copies the view.
const VIEW_TAG: u32 = u32::from_le_bytes(*b"nVEW");
/// Tags of render target and depth views, which hold their record whole (128-byte slots).
const RTV_TAG: u32 = u32::from_le_bytes(*b"nRTV");
const DSV_TAG: u32 = u32::from_le_bytes(*b"nDSV");

/// The recorded views. An index, once handed out, names the same view for the whole run: it is
/// written into the title's descriptor memory, which the title copies as bytes, and the backend
/// keeps per-index tables of its own (`views_from`, `view_at`).
// shortcut: entries are never freed; a view of a resource that is released stays in the table.
// Freeing would need to know that no descriptor slot (in any heap, or any copy the title made of
// one) still holds the index, and the backend's per-index tables to drop the entry. Measured: once
// frames run fast, a title can make ~30k new entries a second (constant and placed views at fresh
// addresses), ~150 bytes each here plus the backend's records.
static VIEWS: std::sync::Mutex<Registry> = std::sync::Mutex::new(Registry::new());

/// The views and a hash index to find one in O(1): `heads` maps a view's hash to the last index
/// with that hash, `next` chains each index to the previous one with the same hash (`NONE` ends
/// it). About 16 bytes per entry next to the view, against a second copy as a map key.
struct Registry {
    views: Vec<View>,
    next: Vec<u32>,
    heads: std::collections::HashMap<u64, u32, std::hash::BuildHasherDefault<Unhashed>>,
}

const NONE: u32 = u32::MAX;

/// Hasher of the map's keys, which are hashes already.
#[derive(Default)]
struct Unhashed(u64);

impl std::hash::Hasher for Unhashed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

impl Registry {
    const fn new() -> Self {
        Registry {
            views: Vec::new(),
            next: Vec::new(),
            heads: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
        }
    }

    /// The index of `view`, added if it is new, and whether it was.
    fn index(&mut self, view: View) -> (usize, bool) {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        view.hash(&mut h);
        let key = h.finish();
        let head = self.heads.get(&key).copied().unwrap_or(NONE);
        let mut i = head;
        while i != NONE {
            if self.views[i as usize] == view {
                return (i as usize, false);
            }
            i = self.next[i as usize];
        }
        let index = self.views.len();
        assert!(index < NONE as usize, "view registry full");
        self.views.push(view);
        self.next.push(head);
        self.heads.insert(key, index as u32);
        (index, true)
    }
}

fn record(handle: u64, view: View) {
    if handle == 0 {
        return;
    }
    let stats = stats::on().then(std::time::Instant::now);
    let kind = match view {
        View::Constant(..) => 0,
        View::Shader(..) => 1,
        View::Unordered(..) => 2,
        View::Sampler(_) | View::ConsoleSampler(_) => 3,
        View::Placed(false, ..) => 4,
        View::Placed(true, ..) => 5,
    };
    let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let locked = stats.map(|t| t.elapsed());
    // Rewriting a descriptor with the same view reuses its entry.
    let (index, new) = views.index(view);
    let len = views.views.len();
    drop(views);
    if let (Some(t), Some(wait)) = (stats, locked) {
        stats::note(wait, t.elapsed(), new.then_some(kind), len);
    }
    // SAFETY: the handle is a descriptor slot of at least 16 bytes the title owns.
    unsafe { (handle as *mut [u32; 2]).write_unaligned([VIEW_TAG, index as u32]) };
}

/// Optional counters of the view registry (`NATIV64_VIEW_STATS=1`): every 30 s, one line with
/// the registry size, records and new entries per second, and the time spent in `record`
/// (lock wait and whole call: total and max over the period), then the new entries of the period
/// by kind: constant, shader, unordered, sampler, placed shader, placed unordered.
mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const PERIOD: Duration = Duration::from_secs(30);

    static START: OnceLock<Option<Instant>> = OnceLock::new();
    /// Milliseconds since `START` of the last line printed.
    static LAST: AtomicU64 = AtomicU64::new(0);
    static RECORDS: AtomicU64 = AtomicU64::new(0);
    static NEW: AtomicU64 = AtomicU64::new(0);
    static NEW_KIND: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
    static WAIT_NS: AtomicU64 = AtomicU64::new(0);
    static WAIT_MAX: AtomicU64 = AtomicU64::new(0);
    static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    static TOTAL_MAX: AtomicU64 = AtomicU64::new(0);

    fn start() -> Option<Instant> {
        *START.get_or_init(|| {
            std::env::var_os("NATIV64_VIEW_STATS")
                .is_some_and(|v| !v.is_empty() && v != "0")
                .then(Instant::now)
        })
    }

    pub fn on() -> bool {
        start().is_some()
    }

    /// `new`: the kind of the view (see the module) when it made a new entry.
    pub fn note(wait: Duration, total: Duration, new: Option<usize>, len: usize) {
        let Some(start) = start() else { return };
        let (w, t) = (wait.as_nanos() as u64, total.as_nanos() as u64);
        RECORDS.fetch_add(1, Relaxed);
        if let Some(k) = new {
            NEW.fetch_add(1, Relaxed);
            NEW_KIND[k].fetch_add(1, Relaxed);
        }
        WAIT_NS.fetch_add(w, Relaxed);
        WAIT_MAX.fetch_max(w, Relaxed);
        TOTAL_NS.fetch_add(t, Relaxed);
        TOTAL_MAX.fetch_max(t, Relaxed);
        let now = start.elapsed().as_millis() as u64;
        let last = LAST.load(Relaxed);
        let span = now.saturating_sub(last);
        if span < PERIOD.as_millis() as u64
            || LAST.compare_exchange(last, now, Relaxed, Relaxed).is_err()
        {
            return;
        }
        let secs = span as f64 / 1000.0;
        let ms = |ns: u64| ns as f64 / 1e6;
        eprintln!(
            "nativ64: view stats {:.0}s: registry {len}, records {:.0}/s, new {:.0}/s, \
             record total {:.1} ms max {:.3} ms, lock wait total {:.1} ms max {:.3} ms, \
             new by kind {:?}",
            now as f64 / 1000.0,
            RECORDS.swap(0, Relaxed) as f64 / secs,
            NEW.swap(0, Relaxed) as f64 / secs,
            ms(TOTAL_NS.swap(0, Relaxed)),
            ms(TOTAL_MAX.swap(0, Relaxed)),
            ms(WAIT_NS.swap(0, Relaxed)),
            ms(WAIT_MAX.swap(0, Relaxed)),
            NEW_KIND.each_ref().map(|n| n.swap(0, Relaxed)),
        );
    }
}

/// How many views are recorded: registry indices are below it.
pub fn view_count() -> usize {
    VIEWS.lock().unwrap_or_else(|e| e.into_inner()).views.len()
}

/// The registry index the view or sampler descriptor at `handle` holds, if it holds one.
///
/// # Safety
/// `handle..handle + 8` is readable.
pub unsafe fn registry_index_at(handle: u64) -> Option<u32> {
    // SAFETY: the caller guarantees the slot is readable.
    let [tag, index] = unsafe { (handle as *const [u32; 2]).read_unaligned() };
    (tag == VIEW_TAG).then_some(index)
}

/// The recorded views from registry index `first` on.
pub fn views_from(first: usize) -> Vec<View> {
    let v = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    v.views
        .get(first..)
        .map(<[View]>::to_vec)
        .unwrap_or_default()
}

/// The view recorded at registry index `index`.
pub fn view_at(index: usize) -> Option<View> {
    VIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .views
        .get(index)
        .cloned()
}

/// The view recorded at `handle`, if one is.
pub fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let [tag, index] = unsafe { (handle as *const [u32; 2]).read_unaligned() };
    if tag != VIEW_TAG {
        return None;
    }
    VIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .views
        .get(index as usize)
        .cloned()
}

fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
    // SAFETY: the title passes a description or NULL.
    (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
}

/// Slot 17, `CreateConstantBufferView(desc, handle)`; `desc` is the address (two words) and size.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_constant_buffer_view(
    _this: *mut c_void,
    desc: *const [u32; 4],
    handle: u64,
) {
    if let Some([lo, hi, size, _]) = words(desc) {
        record(
            handle,
            View::Constant(u64::from(lo) | u64::from(hi) << 32, size),
        );
    }
}

/// Slot 18, `CreateShaderResourceView(resource, desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_shader_resource_view(
    _this: *mut c_void,
    resource: *mut c_void,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(handle, View::Shader(resource as usize, words(desc)));
}

/// Slot 19, `CreateUnorderedAccessView(resource, counter, desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_unordered_access_view(
    _this: *mut c_void,
    resource: *mut c_void,
    counter: *mut c_void,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(
        handle,
        View::Unordered(resource as usize, counter as usize, words(desc)),
    );
}

/// Slot 22, `CreateSampler(desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_sampler(
    _this: *mut c_void,
    desc: *const [u32; 13],
    handle: u64,
) {
    if let Some(d) = words(desc) {
        record(handle, View::Sampler(d));
    }
}

/// Slot 61 (`+0x1e8`), the console's `CreateSamplerX(desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_sampler_x(
    _this: *mut c_void,
    desc: *const [u32; 27],
    handle: u64,
) {
    if let Some(d) = words(desc) {
        record(handle, View::ConsoleSampler(d));
    }
}

/// Slot 70, `CreatePlacedShaderResourceViewX(resource_desc, view_desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_placed_shader_resource_view(
    _this: *mut c_void,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(r), Some(d)) = (words(resource), words(desc)) {
        record(handle, View::Placed(false, r, d));
    }
}

/// Slot 71, `CreatePlacedUnorderedAccessViewX(resource_desc, view_desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_placed_unordered_access_view(
    _this: *mut c_void,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(r), Some(d)) = (words(resource), words(desc)) {
        record(handle, View::Placed(true, r, d));
    }
}

/// A render target or depth view as recorded in its 128-byte slot.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetRecord {
    pub tag: u32,
    /// 1 when the title passed a description.
    pub described: u32,
    pub resource: usize,
    pub desc: [u32; 6],
}

fn record_target(tag: u32, resource: *mut c_void, desc: *const [u32; 6], handle: u64) {
    if handle == 0 {
        return;
    }
    let d = words(desc);
    let r = TargetRecord {
        tag,
        described: u32::from(d.is_some()),
        resource: resource as usize,
        desc: d.unwrap_or_default(),
    };
    // SAFETY: the handle is a 128-byte descriptor slot.
    unsafe { (handle as *mut TargetRecord).write_unaligned(r) };
}

/// Slot 20, `CreateRenderTargetView(resource, desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_render_target_view(
    _this: *mut c_void,
    resource: *mut c_void,
    desc: *const [u32; 6],
    handle: u64,
) {
    record_target(RTV_TAG, resource, desc, handle);
}

/// Slot 21, `CreateDepthStencilView(resource, desc, handle)`.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn create_depth_stencil_view(
    _this: *mut c_void,
    resource: *mut c_void,
    desc: *const [u32; 6],
    handle: u64,
) {
    record_target(DSV_TAG, resource, desc, handle);
}

/// The render target view recorded at `handle`.
pub fn render_target_view(handle: u64) -> Option<TargetRecord> {
    target(handle, RTV_TAG)
}

/// The depth view recorded at `handle`.
pub fn depth_stencil_view(handle: u64) -> Option<TargetRecord> {
    target(handle, DSV_TAG)
}

fn target(handle: u64, tag: u32) -> Option<TargetRecord> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let r = unsafe { (handle as *const TargetRecord).read_unaligned() };
    (r.tag == tag).then_some(r)
}

/// Slot 24, `CopyDescriptorsSimple(count, dst, src, kind)`: bytes are descriptors.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn copy_descriptors_simple(
    _this: *mut c_void,
    count: u32,
    dst: u64,
    src: u64,
    kind: u32,
) {
    let step = get_descriptor_increment_size(std::ptr::null_mut(), kind) as usize;
    if step != 0 && dst != 0 && src != 0 {
        std::ptr::copy(src as *const u8, dst as *mut u8, count as usize * step);
    }
}

/// Slot 23, `CopyDescriptors(dst_ranges, dst_starts, dst_sizes, src_ranges, src_starts,
/// src_sizes, kind)`: the source ranges gathered and scattered over the destinations in order;
/// a NULL size array means one descriptor per range.
///
/// # Safety
/// Descriptions, handles and ranges are NULL or valid, as the title passes them.
pub unsafe extern "win64" fn copy_descriptors(
    _this: *mut c_void,
    dst_ranges: u32,
    dst_starts: *const u64,
    dst_sizes: *const u32,
    src_ranges: u32,
    src_starts: *const u64,
    src_sizes: *const u32,
    kind: u32,
) {
    let step = get_descriptor_increment_size(std::ptr::null_mut(), kind) as u64;
    if step == 0 || dst_starts.is_null() || src_starts.is_null() {
        return;
    }
    let range = |starts: *const u64, sizes: *const u32, i: usize| {
        (
            *starts.add(i),
            if sizes.is_null() { 1 } else { *sizes.add(i) },
        )
    };
    let mut dst = (0..dst_ranges as usize).map(|i| range(dst_starts, dst_sizes, i));
    let (mut to, mut room) = dst.next().unwrap_or((0, 0));
    for i in 0..src_ranges as usize {
        let (mut from, mut left) = range(src_starts, src_sizes, i);
        while left > 0 {
            while room == 0 {
                let Some(next) = dst.next() else { return };
                (to, room) = next;
            }
            let n = left.min(room);
            let bytes = u64::from(n) * step;
            std::ptr::copy(from as *const u8, to as *mut u8, bytes as usize);
            (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::object::S_FALSE;
    use crate::xobj::{E_NOINTERFACE, S_OK};

    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(d: HeapDesc, p: Placement, out: &mut *mut c_void) -> HRESULT {
        let e = Extended(d, p);
        unsafe { create_descriptor_heap(std::ptr::null_mut(), &e.0, &DESCRIPTOR_HEAP_IIDS[0], out) }
    }

    #[test]
    fn heap_owns_zeroed_memory() {
        unsafe {
            let d = HeapDesc {
                kind: 0,
                count: 1000,
                flags: 1,
                node_mask: 0,
            };
            let none = Placement {
                address: 0,
                size: 0,
            };
            let mut h = std::ptr::null_mut();
            assert_eq!(create(d, none, &mut h), S_OK);
            let cpu = get_start(h);
            assert_eq!(cpu % 64, 0);
            assert_eq!(get_start(h), cpu);
            // the last descriptor's last byte is ours to write, and starts zeroed
            let last = (cpu as *mut u8).add(999 * 32 + 31);
            assert_eq!(last.read(), 0);
            last.write(1);
            let mut got = HeapDesc::default();
            assert_eq!(get_desc(&mut got, h), &mut got as *mut _);
            assert_eq!(got, d);
            // through the vtable
            let vtbl = *(h as *const *const *const ());
            let f: unsafe extern "win64" fn(*mut c_void) -> u64 =
                std::mem::transmute(*vtbl.add(10));
            assert_eq!(f(h), cpu);
            assert_eq!(XObject::<DescriptorHeapData>::release(h), 0);
        }
    }

    #[test]
    fn placed_heap_uses_the_titles_memory() {
        unsafe {
            let d = HeapDesc {
                kind: 1,
                count: 8,
                ..Default::default()
            };
            let mut h = std::ptr::null_mut();
            let at = Placement {
                address: 0x4000_0000,
                size: 8 * 16,
            };
            assert_eq!(create(d, at, &mut h), S_OK);
            assert_eq!(get_start(h), 0x4000_0000);
            XObject::<DescriptorHeapData>::release(h);
            // a size that does not match the count is not a placement
            let wrong = Placement {
                address: 0x4000_0000,
                size: 5,
            };
            assert_eq!(create(d, wrong, &mut h), S_OK);
            assert_ne!(get_start(h), 0x4000_0000);
            XObject::<DescriptorHeapData>::release(h);
        }
    }

    #[test]
    fn creation_errors() {
        unsafe {
            let none = Placement {
                address: 0,
                size: 0,
            };
            let mut h = std::ptr::dangling_mut::<c_void>();
            let bad = HeapDesc {
                kind: 9,
                count: 4,
                ..Default::default()
            };
            assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
            assert!(h.is_null());
            let n = std::ptr::null_mut();
            let d = HeapDesc {
                kind: 2,
                count: 4,
                ..Default::default()
            };
            assert_eq!(
                create_descriptor_heap(n, std::ptr::null(), &DESCRIPTOR_HEAP_IIDS[0], &mut h),
                E_INVALIDARG
            );
            let other = guid(1, 2, 3, [0; 8]);
            assert_eq!(create_descriptor_heap(n, &d, &other, &mut h), E_NOINTERFACE);
            assert_eq!(
                create_descriptor_heap(n, &d, &DESCRIPTOR_HEAP_IIDS[0], std::ptr::null_mut()),
                S_FALSE
            );
            // a placement that wraps the address space
            let wrap = Placement {
                address: u64::MAX - 8,
                size: 4 * 128,
            };
            assert_eq!(create(d, wrap, &mut h), E_INVALIDARG);
        }
    }

    #[test]
    fn registry_dedups_and_keeps_indices() {
        let mut r = Registry::new();
        let placed = |n: u32| View::Placed(n.is_multiple_of(2), [n; 14], [n.wrapping_mul(7); 16]);
        let first: Vec<usize> = (0..5000).map(|n| r.index(placed(n)).0).collect();
        assert_eq!(first, (0..5000).collect::<Vec<_>>());
        for n in (0..5000).rev() {
            assert_eq!(r.index(placed(n)), (n as usize, false));
        }
        assert_eq!(r.index(View::Constant(1, 2)), (5000, true));
        assert_eq!(r.index(View::Constant(1, 3)), (5001, true));
        assert_eq!(r.index(View::Constant(1, 2)), (5000, false));
        assert_eq!(r.views.len(), 5002);
        assert_eq!(r.views[17], placed(17));
    }

    #[test]
    fn registry_chains_equal_hashes() {
        // Two views forced under one hash key: both found, each at its own index.
        let mut r = Registry::new();
        let (a, b) = (View::Constant(1, 1), View::Constant(2, 2));
        r.views.extend([a.clone(), b.clone()]);
        r.next.extend([NONE, 0]);
        let (ka, kb) = {
            use std::hash::{Hash, Hasher};
            let k = |v: &View| {
                let mut h = std::hash::DefaultHasher::new();
                v.hash(&mut h);
                h.finish()
            };
            (k(&a), k(&b))
        };
        r.heads.insert(ka, 1);
        r.heads.insert(kb, 1);
        assert_eq!(r.index(b), (1, false));
        assert_eq!(r.index(a), (0, false));
    }

    #[test]
    fn concurrent_records_agree_on_indices() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let threads: Vec<_> = (0..8u32)
                .map(|t| {
                    std::thread::spawn(move || {
                        let mut slot = [0u64; 4];
                        let mut got = Vec::new();
                        for n in 0..20_000u32 {
                            let v = View::Placed(true, [0x5eed_0000 | (n % 3000); 14], [t % 2; 16]);
                            record(slot.as_mut_ptr() as u64, v.clone());
                            let index = unsafe { registry_index_at(slot.as_ptr() as u64) };
                            assert_eq!(view_at(index.unwrap() as usize), Some(v));
                            got.push(index.unwrap());
                        }
                        got
                    })
                })
                .collect();
            let got: Vec<Vec<u32>> = threads.into_iter().map(|h| h.join().unwrap()).collect();
            tx.send(got).unwrap();
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("records took over 60 s");
        // Threads with the same parity recorded the same views: the same indices.
        for t in 2..8 {
            assert_eq!(got[t], got[t % 2]);
        }
        assert_ne!(got[0][0], got[1][0]);
    }

    #[test]
    fn views_are_recorded_and_survive_copies() {
        unsafe {
            let n = std::ptr::null_mut();
            let heap = [0u64; 4 * 4]; // four 32-byte slots
            let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
            let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
            assert_eq!(view(s0), None);
            create_constant_buffer_view(n, &[0x1000, 1, 256, 0], s0);
            assert_eq!(view(s0), Some(View::Constant(0x1_0000_1000, 256)));
            create_shader_resource_view(n, 0x77 as *mut c_void, std::ptr::null(), s1);
            assert_eq!(view(s1), Some(View::Shader(0x77, None)));
            let d = [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 10];
            create_unordered_access_view(n, 0x78 as *mut c_void, std::ptr::null_mut(), &d, s2);
            assert_eq!(view(s2), Some(View::Unordered(0x78, 0, Some(d))));
            create_sampler(n, &[7; 13], s3);
            assert_eq!(view(s3), Some(View::Sampler([7; 13])));
            create_sampler_x(n, &[9; 27], s3);
            assert_eq!(view(s3), Some(View::ConsoleSampler([9; 27])));
            create_sampler_x(n, std::ptr::null(), s3);
            assert_eq!(view(s3), Some(View::ConsoleSampler([9; 27])));
            create_sampler(n, &[7; 13], s3);
            // placed views keep both descriptions
            create_placed_shader_resource_view(n, &[3; 14], &[4; 16], s1);
            assert_eq!(view(s1), Some(View::Placed(false, [3; 14], [4; 16])));
            create_placed_unordered_access_view(n, &[3; 14], &[4; 16], s1);
            assert_eq!(view(s1), Some(View::Placed(true, [3; 14], [4; 16])));
            create_placed_shader_resource_view(n, std::ptr::null(), &[4; 16], s1);
            assert_eq!(view(s1), Some(View::Placed(true, [3; 14], [4; 16])));
            create_shader_resource_view(n, 0x77 as *mut c_void, std::ptr::null(), s1);
            // a NULL description is no view; NULL handle is ignored
            create_constant_buffer_view(n, std::ptr::null(), s0);
            create_sampler(n, &[7; 13], 0);
            assert_eq!(view(0), None);
            // copies carry views
            copy_descriptors_simple(n, 2, s2, s0, 0);
            assert_eq!(view(s2), view(s0));
            // two descriptors from s0 also took s1 over s3
            assert_eq!(view(s3), view(s1));
            let (ds, dz) = ([s3, s0], [1u32, 1]);
            let (ss, sz) = ([s1], [2u32]);
            copy_descriptors(
                n,
                2,
                ds.as_ptr(),
                dz.as_ptr(),
                1,
                ss.as_ptr(),
                sz.as_ptr(),
                0,
            );
            assert_eq!(view(s3), Some(View::Shader(0x77, None)));
            assert_eq!(view(s0), view(s2));
            // unknown kind: nothing copied
            copy_descriptors_simple(n, 1, s1, s3, 9);
            copy_descriptors(
                n,
                0,
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
        }
    }

    #[test]
    fn target_views_hold_their_record() {
        unsafe {
            let n = std::ptr::null_mut();
            let slot = [0u8; 128];
            let h = slot.as_ptr() as u64;
            create_render_target_view(n, 0x55 as *mut c_void, &[1, 2, 3, 4, 5, 6], h);
            let r = render_target_view(h).unwrap();
            assert_eq!(
                (r.resource, r.described, r.desc),
                (0x55, 1, [1, 2, 3, 4, 5, 6])
            );
            assert_eq!(depth_stencil_view(h), None);
            create_depth_stencil_view(n, 0x56 as *mut c_void, std::ptr::null(), h);
            assert_eq!(render_target_view(h), None);
            let d = depth_stencil_view(h).unwrap();
            assert_eq!((d.resource, d.described), (0x56, 0));
            create_render_target_view(n, 0x55 as *mut c_void, std::ptr::null(), 0);
            assert_eq!(render_target_view(0), None);
        }
    }
}
