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
        move || {
            if plain.kind <= 1 {
                VIEWS.add_heap(root(plain.kind, start, size, memory.is_some()));
            }
            DescriptorHeapData {
                bits: 0,
                console_refs: 0,
                desc: plain,
                start,
                memory: memory.take(),
            }
        },
        iid,
        out,
    )
}

/// The root a heap of views (kind 0) or samplers (kind 1) is to the sweep.
fn root(kind: u32, start: u64, size: u64, owned: bool) -> Root {
    Root {
        start,
        end: start + size,
        stride: step(kind),
        owned,
    }
}

fn step(kind: u32) -> u64 {
    // SAFETY: reads no object.
    unsafe { get_descriptor_increment_size(std::ptr::null_mut(), kind) as u64 }
}

impl Drop for DescriptorHeapData {
    fn drop(&mut self) {
        if self.desc.kind <= 1 {
            let size = step(self.desc.kind) * u64::from(self.desc.count.max(1));
            VIEWS.remove_heap(root(
                self.desc.kind,
                self.start,
                size,
                self.memory.is_some(),
            ));
        }
    }
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

/// The recorded views (`docs/gpu.md`, "Reclaiming view registry indices"). An index is written
/// into the title's descriptor memory, which the title copies as bytes, so it cannot be
/// refcounted: a sweep ([`sweep`]) reads every slot that can hold one (the roots: the heaps of
/// views and samplers, and the slots outside them that `record` wrote) and frees the indices
/// that no slot held in two sweeps in a row. The backend learns which indices got a view or
/// lost theirs from the change log ([`Consumer::changes`]).
// shortcut: a slot outside every heap is a root only once `record` wrote it; an index the title
// keeps, by its own copy, in memory outside the heaps for longer than two sweeps is freed under
// it (no title seen doing so yet).
static VIEWS: Views = Views::new();

/// A heap of views or samplers the sweep reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Root {
    start: u64,
    end: u64,
    /// Bytes between descriptors.
    stride: u64,
    /// The heap's own memory (else the title's: read where committed).
    owned: bool,
}

/// The registry and what its sweep reads.
struct Views {
    reg: std::sync::Mutex<Registry>,
    /// The live heaps of views and samplers, sorted by start.
    heaps: std::sync::RwLock<Vec<Root>>,
    /// The slots outside every heap that `record` wrote and that still held a tag at the last
    /// sweep.
    outside: std::sync::Mutex<std::collections::HashSet<u64, std::hash::BuildHasherDefault<Mixed>>>,
    /// Between the first and the last step of a sweep: `record` and copies note in
    /// `Registry::touched` the indices they hand out. Set and cleared under the registry lock.
    sweeping: std::sync::atomic::AtomicBool,
}

/// The views and a hash index to find one in O(1): `heads` maps a view's hash to the last index
/// with that hash, `next` chains each index to the previous one with the same hash (`NONE` ends
/// it). About 16 bytes per entry next to the view, against a second copy as a map key.
struct Registry {
    /// None: a free index.
    views: Vec<Option<View>>,
    next: Vec<u32>,
    heads: std::collections::HashMap<u64, u32, std::hash::BuildHasherDefault<Unhashed>>,
    /// The free indices, the lowest first out (the live range stays compact).
    free: std::collections::BinaryHeap<std::cmp::Reverse<u32>>,
    live: usize,
    /// Bitset: in use and held by no root at the last sweep (freed if the next agrees).
    quarantine: Vec<u64>,
    /// Bitset, during a sweep: the indices `record` and copies handed out since its start.
    touched: Vec<u64>,
    /// Indices given a view (new or reused) since the last sweep.
    added: usize,
    /// Indices that got a view or lost theirs, in order; `log_base` numbers `log[0]`.
    log: std::collections::VecDeque<u32>,
    log_base: u64,
    /// By consumer id: None, an unused id; `Some(None)`, a consumer that reads every index at
    /// its next call; `Some(Some(n))`, one that has read the log up to number `n`.
    cursors: Vec<Option<Option<u64>>>,
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

/// Hasher of slot addresses (aligned: their low bits say little).
#[derive(Default)]
struct Mixed(u64);

impl std::hash::Hasher for Mixed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(b));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (n ^ n >> 29).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

fn bit(set: &[u64], i: usize) -> bool {
    set.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 != 0)
}

fn set_bit(set: &mut [u64], i: usize) {
    if let Some(w) = set.get_mut(i / 64) {
        *w |= 1 << (i % 64);
    }
}

fn clear_bit(set: &mut [u64], i: usize) {
    if let Some(w) = set.get_mut(i / 64) {
        *w &= !(1 << (i % 64));
    }
}

fn key(view: &View) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    view.hash(&mut h);
    h.finish()
}

/// The registry index the slot at `slot` holds, if it holds one.
///
/// # Safety
/// `slot..slot + 8` is readable (racy writes by other threads are fine).
unsafe fn tagged(slot: u64) -> Option<u32> {
    // SAFETY: as the caller guarantees.
    let [tag, index] = unsafe { (slot as *const [u32; 2]).read_unaligned() };
    (tag == VIEW_TAG).then_some(index)
}

/// The committed run `(start, end)` of title memory around `addr`.
fn committed(addr: u64) -> Option<(u64, u64)> {
    nativ64::api_ms_win_core_memory::committed_run(addr as usize).map(|(a, b)| (a as u64, b as u64))
}

impl Registry {
    const fn new() -> Self {
        Registry {
            views: Vec::new(),
            next: Vec::new(),
            heads: std::collections::HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
            free: std::collections::BinaryHeap::new(),
            live: 0,
            quarantine: Vec::new(),
            touched: Vec::new(),
            added: 0,
            log: std::collections::VecDeque::new(),
            log_base: 0,
            cursors: Vec::new(),
        }
    }

    /// The index of `view`, given one (the lowest free, else a new one) if it has none, and
    /// whether it was.
    fn index(&mut self, view: View) -> (usize, bool) {
        let key = key(&view);
        let head = self.heads.get(&key).copied().unwrap_or(NONE);
        let mut i = head;
        while i != NONE {
            if self.views[i as usize].as_ref() == Some(&view) {
                return (i as usize, false);
            }
            i = self.next[i as usize];
        }
        let index = match self.free.pop() {
            Some(std::cmp::Reverse(i)) => i as usize,
            None => {
                let i = self.views.len();
                assert!(i < NONE as usize, "view registry full");
                self.views.push(None);
                self.next.push(NONE);
                i
            }
        };
        self.views[index] = Some(view);
        self.next[index] = head;
        self.heads.insert(key, index as u32);
        self.live += 1;
        self.added += 1;
        self.note(index);
        (index, true)
    }

    /// Frees `index`: out of its hash chain, its view dropped, on the free list.
    fn free(&mut self, index: usize) {
        let Some(view) = self.views[index].take() else {
            return;
        };
        let key = key(&view);
        let after = self.next[index];
        match self.heads.get(&key).copied() {
            Some(h) if h as usize == index => {
                if after == NONE {
                    self.heads.remove(&key);
                } else {
                    self.heads.insert(key, after);
                }
            }
            Some(mut j) => {
                while j != NONE {
                    let n = self.next[j as usize];
                    if n as usize == index {
                        self.next[j as usize] = after;
                        break;
                    }
                    j = n;
                }
            }
            None => {}
        }
        self.next[index] = NONE;
        self.free.push(std::cmp::Reverse(index as u32));
        self.live -= 1;
        clear_bit(&mut self.quarantine, index);
        self.note(index);
    }

    /// Logs a change of `index` for the consumers that read the log.
    fn note(&mut self, index: usize) {
        if !self.cursors.iter().any(|c| matches!(c, Some(Some(_)))) {
            self.log_base += self.log.len() as u64 + 1;
            self.log.clear();
            return;
        }
        self.log.push_back(index as u32);
        // A consumer that stopped reading must not keep the log forever: past the cap it
        // reads every index at its next call instead.
        let cap = (1usize << 20).max(2 * self.views.len());
        if self.log.len() > cap {
            let n = self.log.len() / 2;
            self.log.drain(..n);
            self.log_base += n as u64;
        }
    }

    /// Drops the log entries every consumer has read.
    fn trim(&mut self) {
        let end = self.log_base + self.log.len() as u64;
        let read = self
            .cursors
            .iter()
            .filter_map(|c| match c {
                Some(Some(n)) => Some(*n),
                _ => None,
            })
            .min()
            .unwrap_or(end)
            .max(self.log_base);
        self.log.drain(..(read - self.log_base) as usize);
        self.log_base = read;
    }

    fn add_consumer(&mut self) -> usize {
        let id = match self.cursors.iter().position(Option::is_none) {
            Some(id) => id,
            None => {
                self.cursors.push(None);
                self.cursors.len() - 1
            }
        };
        self.cursors[id] = Some(None);
        id
    }

    fn remove_consumer(&mut self, id: usize) {
        if let Some(c) = self.cursors.get_mut(id) {
            *c = None;
        }
        self.trim();
    }

    /// The indices changed since consumer `id` last asked (every index at its first call),
    /// once each, lowest first, with their view (None: freed).
    fn changes(&mut self, id: usize) -> Vec<(u32, Option<View>)> {
        let end = self.log_base + self.log.len() as u64;
        let from = match self.cursors.get(id) {
            Some(Some(Some(n))) if *n >= self.log_base => Some(*n),
            _ => None,
        };
        let out = match from {
            None => (0..self.views.len())
                .map(|i| (i as u32, self.views[i].clone()))
                .collect(),
            Some(n) => {
                let mut ix: Vec<u32> = self
                    .log
                    .range((n - self.log_base) as usize..)
                    .copied()
                    .collect();
                ix.sort_unstable();
                ix.dedup();
                ix.into_iter()
                    .map(|i| (i, self.views[i as usize].clone()))
                    .collect()
            }
        };
        if let Some(c @ Some(_)) = self.cursors.get_mut(id) {
            *c = Some(Some(end));
        }
        self.trim();
        out
    }
}

/// What a sweep did (`NATIV64_VIEW_STATS` prints it).
#[derive(Debug, Default)]
struct Swept {
    live: usize,
    free: usize,
    quarantined: usize,
    freed: usize,
    /// Heap bytes read, outside slots read and dropped from the set.
    heap_bytes: u64,
    outside: usize,
    dropped: usize,
    mark: std::time::Duration,
    settle: std::time::Duration,
}

/// Between the first and the last step of a sweep.
struct Sweep {
    len: usize,
    marked: Vec<u64>,
    swept: Swept,
}

impl Views {
    const fn new() -> Self {
        Views {
            reg: std::sync::Mutex::new(Registry::new()),
            heaps: std::sync::RwLock::new(Vec::new()),
            outside: std::sync::Mutex::new(std::collections::HashSet::with_hasher(
                std::hash::BuildHasherDefault::new(),
            )),
            sweeping: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn reg(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add_heap(&self, root: Root) {
        let mut h = self.heaps.write().unwrap_or_else(|e| e.into_inner());
        let at = h.partition_point(|r| r.start <= root.start);
        h.insert(at, root);
    }

    fn remove_heap(&self, root: Root) {
        let mut h = self.heaps.write().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = h.iter().position(|r| *r == root) {
            h.remove(at);
        }
    }

    fn in_heap(&self, slot: u64) -> bool {
        let h = self.heaps.read().unwrap_or_else(|e| e.into_inner());
        let at = h.partition_point(|r| r.start <= slot);
        h[..at].iter().rev().any(|r| slot < r.end)
    }

    fn record(&self, handle: u64, view: View) {
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
        let outside = !self.in_heap(handle);
        let mut reg = self.reg();
        let locked = stats.map(|t| t.elapsed());
        // Rewriting a descriptor with the same view reuses its entry.
        let (index, new) = reg.index(view);
        if self.sweeping.load(std::sync::atomic::Ordering::SeqCst) {
            set_bit(&mut reg.touched, index);
        }
        let len = reg.views.len();
        // Under the lock: a sweep that starts after it reads the slot written; one that started
        // before has the index touched. (Written after the lock, a thread paused in between
        // could see a reused index freed under it.)
        // SAFETY: the handle is a descriptor slot of at least 16 bytes the title owns.
        unsafe { (handle as *mut [u32; 2]).write_unaligned([VIEW_TAG, index as u32]) };
        // Also under the lock: a sweep that starts after it finds the slot in the set. A sweep
        // drops a slot only after reading it again under the set's lock.
        if outside {
            self.outside
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(handle);
        }
        drop(reg);
        if let (Some(t), Some(wait)) = (stats, locked) {
            stats::note(wait, t.elapsed(), new.then_some(kind), len);
        }
    }

    /// After a copy of `bytes` bytes of descriptors to `dst`: during a sweep, the indices
    /// copied stay live.
    fn copied(&self, dst: u64, bytes: u64, stride: u64) {
        use std::sync::atomic::{fence, Ordering::SeqCst};
        // The copy visible before the flag is read: a sweep that starts after this read sees
        // the destination.
        fence(SeqCst);
        if stride == 0 || !self.sweeping.load(SeqCst) {
            return;
        }
        // SAFETY: the destination was just written, so it is readable.
        let ix: Vec<u32> = (0..bytes / stride)
            .filter_map(|k| unsafe { tagged(dst + k * stride) })
            .collect();
        if ix.is_empty() {
            return;
        }
        let mut reg = self.reg();
        if self.sweeping.load(SeqCst) {
            for i in ix {
                set_bit(&mut reg.touched, i as usize);
            }
        }
    }

    fn view_at(&self, index: usize) -> Option<View> {
        self.reg().views.get(index).cloned().flatten()
    }

    /// Step 1 (`force`: even if not due).
    fn begin(&self, force: bool) -> Option<Sweep> {
        let mut reg = self.reg();
        if !force && reg.added < 65_536.max(reg.live / 2) {
            return None;
        }
        let len = reg.views.len();
        reg.added = 0;
        reg.touched.clear();
        reg.touched.resize(len.div_ceil(64), 0);
        self.sweeping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Some(Sweep {
            len,
            marked: vec![0; len.div_ceil(64)],
            swept: Swept::default(),
        })
    }

    /// Step 2, without the registry lock: marks the indices every root holds.
    fn mark(&self, s: &mut Sweep) {
        let t = std::time::Instant::now();
        let len = s.len;
        let marked = &mut s.marked;
        let mut mark = |slot: u64| {
            // SAFETY: the callers pass readable slots.
            let index = unsafe { tagged(slot) };
            if let Some(i) = index.filter(|&i| (i as usize) < len) {
                set_bit(marked, i as usize);
            }
            index.is_some()
        };
        {
            let heaps = self.heaps.read().unwrap_or_else(|e| e.into_inner());
            for r in heaps.iter() {
                let mut a = r.start;
                while a < r.end {
                    let end = if r.owned {
                        r.end
                    } else {
                        match committed(a) {
                            Some((_, e)) => e.min(r.end),
                            None => {
                                a = (a | (HEAP_PAGE as u64 - 1)) + 1;
                                continue;
                            }
                        }
                    };
                    let mut slot = r.start + (a - r.start).div_ceil(r.stride) * r.stride;
                    while slot + 8 <= end {
                        mark(slot);
                        slot += r.stride;
                    }
                    s.swept.heap_bytes += end - a;
                    a = end;
                }
            }
        }
        let mut slots: Vec<u64> = self
            .outside
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        slots.sort_unstable();
        s.swept.outside = slots.len();
        let mut run = (0, 0);
        let mut gone = Vec::new();
        for slot in slots {
            if !(run.0 <= slot && slot + 8 <= run.1) {
                run = committed(slot).unwrap_or((0, 0));
                if !(run.0 <= slot && slot + 8 <= run.1) {
                    gone.push(slot);
                    continue;
                }
            }
            if !mark(slot) {
                gone.push(slot);
            }
        }
        if !gone.is_empty() {
            let mut set = self.outside.lock().unwrap_or_else(|e| e.into_inner());
            for slot in gone {
                // Read again under the lock: `record` may have written it since.
                let readable = committed(slot).is_some_and(|(a, b)| a <= slot && slot + 8 <= b);
                // SAFETY: committed.
                if !(readable && unsafe { tagged(slot) }.is_some()) {
                    set.remove(&slot);
                    s.swept.dropped += 1;
                }
            }
        }
        s.swept.mark = t.elapsed();
    }

    /// Step 3: frees the indices absent from this sweep and the last, quarantines the ones
    /// absent from this one only.
    fn settle(&self, s: Sweep) -> Swept {
        let t = std::time::Instant::now();
        let mut swept = s.swept;
        let mut reg = self.reg();
        let words = reg.views.len().div_ceil(64);
        reg.quarantine.resize(words, 0);
        for i in 0..s.len {
            if reg.views[i].is_none() {
                continue;
            }
            let live = bit(&s.marked, i) || bit(&reg.touched, i);
            match (live, bit(&reg.quarantine, i)) {
                (false, true) => {
                    reg.free(i);
                    swept.freed += 1;
                }
                (false, false) => set_bit(&mut reg.quarantine, i),
                (true, true) => clear_bit(&mut reg.quarantine, i),
                (true, false) => {}
            }
        }
        self.sweeping
            .store(false, std::sync::atomic::Ordering::SeqCst);
        reg.touched.clear();
        swept.live = reg.live;
        swept.free = reg.free.len();
        swept.quarantined = reg.quarantine.iter().map(|w| w.count_ones() as usize).sum();
        swept.settle = t.elapsed();
        swept
    }

    fn sweep(&self, force: bool) -> Option<Swept> {
        let mut s = self.begin(force)?;
        self.mark(&mut s);
        Some(self.settle(s))
    }
}

fn record(handle: u64, view: View) {
    VIEWS.record(handle, view);
}

/// Frees the registry indices no descriptor slot held in this sweep and the last, when enough
/// entries were made since the last sweep. Called at the start of each submission under the
/// backend's lock, its queue idle (the backend learns of the changes through
/// [`Consumer::changes`] before it uses an index).
pub fn sweep() {
    let Some(s) = VIEWS.sweep(false) else { return };
    if stats::on() {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
        eprintln!(
            "nativ64: view stats sweep {:.0}s: live {}, free {}, quarantined {}, freed {}, \
             roots {} heap bytes + {} outside slots ({} dropped), mark {:.1} ms, settle {:.1} ms",
            stats::secs(),
            s.live,
            s.free,
            s.quarantined,
            s.freed,
            s.heap_bytes,
            s.outside,
            s.dropped,
            ms(s.mark),
            ms(s.settle),
        );
    }
}

/// A reader of the change log: what it built per registry index is kept up to date by
/// [`Consumer::changes`].
pub struct Consumer {
    views: &'static Views,
    id: usize,
}

impl Consumer {
    pub fn new() -> Consumer {
        Consumer::of(&VIEWS)
    }

    fn of(views: &'static Views) -> Consumer {
        let id = views.reg().add_consumer();
        Consumer { views, id }
    }

    /// The registry indices that got a view or lost theirs since the last call (every index
    /// at the first), once each, lowest first, with their view now (None: freed).
    pub fn changes(&self) -> Vec<(u32, Option<View>)> {
        self.views.reg().changes(self.id)
    }
}

impl Default for Consumer {
    fn default() -> Self {
        Consumer::new()
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        self.views.reg().remove_consumer(self.id);
    }
}

/// Optional counters of the view registry (`NATIV64_VIEW_STATS=1`): every 30 s, one line with
/// the registry size, records and new entries per second, and the time spent in `record`
/// (lock wait and whole call: total and max over the period), then the new entries of the period
/// by kind: constant, shader, unordered, sampler, placed shader, placed unordered. And one line
/// per sweep (`sweep`).
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

    /// Seconds since the counters started.
    pub fn secs() -> f64 {
        start().map_or(0.0, |s| s.elapsed().as_secs_f64())
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

/// The length of the registry: every index is below it (some may be free).
pub fn view_capacity() -> usize {
    VIEWS.reg().views.len()
}

/// The registry index the view or sampler descriptor at `handle` holds, if it holds one.
///
/// # Safety
/// `handle..handle + 8` is readable.
pub unsafe fn registry_index_at(handle: u64) -> Option<u32> {
    // SAFETY: the caller guarantees the slot is readable.
    unsafe { tagged(handle) }
}

/// The view recorded at registry index `index` (None: free).
pub fn view_at(index: usize) -> Option<View> {
    VIEWS.view_at(index)
}

/// The view recorded at `handle`, if one is.
pub fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let index = unsafe { tagged(handle) }?;
    VIEWS.view_at(index as usize)
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
        VIEWS.copied(dst, (count as usize * step) as u64, step as u64);
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
            VIEWS.copied(to, bytes, step);
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
        assert_eq!(r.views[17], Some(placed(17)));
    }

    #[test]
    fn registry_chains_equal_hashes() {
        // Two views forced under one hash key: both found, each at its own index.
        let mut r = Registry::new();
        let (a, b) = (View::Constant(1, 1), View::Constant(2, 2));
        r.views.extend([Some(a.clone()), Some(b.clone())]);
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

    use nativ64::api_ms_win_core_memory as table;

    /// Committed title memory of `bytes` bytes (a page at least).
    fn title_slots(bytes: usize) -> u64 {
        table::alloc(
            0,
            bytes.max(4096),
            table::MEM_RESERVE | table::MEM_COMMIT,
            0x04,
            None,
        )
        .unwrap() as u64
    }

    fn constant(n: u64) -> View {
        View::Constant(0xc0de_0000_0000 + n * 256, 256)
    }

    fn index_in(slot: u64) -> usize {
        // SAFETY: a slot of the test's.
        unsafe { tagged(slot) }.unwrap() as usize
    }

    #[test]
    fn an_index_is_freed_after_two_sweeps_without_a_holder() {
        static V: Views = Views::new();
        let s = title_slots(4096);
        V.record(s, constant(1));
        let a = index_in(s);
        assert_eq!(V.sweep(true).unwrap().freed, 0);
        // the slot overwritten: no slot holds `a`
        V.record(s, constant(2));
        let sw = V.sweep(true).unwrap();
        assert_eq!((sw.freed, sw.quarantined), (0, 1));
        assert_eq!(V.view_at(a), Some(constant(1)), "quarantined, not freed");
        // found again by dedup while quarantined, and held: out of quarantine
        V.record(s + 32, constant(1));
        assert_eq!(index_in(s + 32), a);
        let sw = V.sweep(true).unwrap();
        assert_eq!((sw.freed, sw.quarantined), (0, 0));
        // unheld again: freed at the second sweep
        V.record(s + 32, constant(3));
        assert_eq!(V.sweep(true).unwrap().freed, 0);
        let sw = V.sweep(true).unwrap();
        assert_eq!((sw.freed, sw.live, sw.free), (1, 2, 1));
        assert_eq!(V.view_at(a), None);
        assert_eq!(V.view_at(index_in(s)), Some(constant(2)));
        // dedup no longer finds it: recorded again, it takes the free index anew
        V.record(s + 64, constant(1));
        assert_eq!(index_in(s + 64), a);
        assert_eq!(V.sweep(false).map(|s| s.freed), None, "not due");
        table::release(s as usize).unwrap();
    }

    #[test]
    fn indices_held_by_a_slot_are_never_freed() {
        static V: Views = Views::new();
        let mut own = vec![0u64; 4 * 8].into_boxed_slice();
        let own_at = own.as_mut_ptr() as u64;
        V.add_heap(Root {
            start: own_at,
            end: own_at + 256,
            stride: 32,
            owned: true,
        });
        let placed = title_slots(8192);
        V.add_heap(Root {
            start: placed,
            end: placed + 8192,
            stride: 16,
            owned: false,
        });
        let outside = title_slots(4096);
        V.record(own_at + 32 * 3, constant(10));
        V.record(placed + 16 * 300, View::Sampler([5; 13]));
        V.record(outside + 64, constant(11));
        let held = [own_at + 32 * 3, placed + 16 * 300, outside + 64].map(index_in);
        assert_eq!(
            V.outside
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [outside + 64]
        );
        for _ in 0..4 {
            let sw = V.sweep(true).unwrap();
            assert_eq!((sw.freed, sw.quarantined, sw.outside), (0, 0, 1));
            assert_eq!(sw.heap_bytes, 256 + 8192);
        }
        for i in held {
            assert!(V.view_at(i).is_some());
        }
        // the outside slot's memory released: dropped from the set, its index freed
        table::release(outside as usize).unwrap();
        assert_eq!(V.sweep(true).unwrap().dropped, 1);
        assert!(V.outside.lock().unwrap().is_empty());
        assert_eq!(V.sweep(true).unwrap().freed, 1);
        assert_eq!(V.view_at(held[2]), None);
        // a heap dropped: its slots are no root any more
        V.remove_heap(Root {
            start: own_at,
            end: own_at + 256,
            stride: 32,
            owned: true,
        });
        V.sweep(true);
        assert_eq!(V.sweep(true).unwrap().freed, 1);
        assert_eq!(V.view_at(held[0]), None);
        assert!(V.view_at(held[1]).is_some());
        drop(own);
        table::release(placed as usize).unwrap();
    }

    #[test]
    fn records_and_copies_during_a_sweep_keep_the_index() {
        static V: Views = Views::new();
        let s = title_slots(4096);
        V.record(s, constant(20));
        let a = index_in(s);
        V.record(s, constant(21));
        assert_eq!(V.sweep(true).unwrap().quarantined, 1);
        // a record hands `a` out during the sweep, into a slot cleared before the sweep reads
        // it (moved where no sweep looks): kept, out of quarantine
        let mut sw = V.begin(true).unwrap();
        V.record(s + 32, constant(20));
        assert_eq!(index_in(s + 32), a);
        // SAFETY: the test's slot.
        unsafe { ((s + 32) as *mut [u32; 8]).write([0; 8]) };
        V.mark(&mut sw);
        let r = V.settle(sw);
        assert_eq!((r.freed, r.quarantined), (0, 0));
        // the same with a copy of a slot outside the roots
        assert_eq!(V.sweep(true).unwrap().quarantined, 1);
        let stash = Box::new([VIEW_TAG, a as u32, 0, 0, 0, 0, 0, 0]);
        let mut sw = V.begin(true).unwrap();
        // SAFETY: 32 bytes each.
        unsafe {
            std::ptr::copy(stash.as_ptr().cast::<u8>(), (s + 64) as *mut u8, 32);
            V.copied(s + 64, 32, 32);
            ((s + 64) as *mut [u32; 8]).write([0; 8]);
        }
        V.mark(&mut sw);
        let r = V.settle(sw);
        assert_eq!((r.freed, r.quarantined), (0, 0));
        assert_eq!(V.view_at(a), Some(constant(20)));
        // and without either, two sweeps free it
        V.sweep(true);
        assert_eq!(V.sweep(true).unwrap().freed, 1);
        table::release(s as usize).unwrap();
    }

    #[test]
    fn reuse_takes_the_lowest_free_index() {
        static V: Views = Views::new();
        let s = title_slots(4096);
        for n in 0..5 {
            V.record(s + 32 * n, constant(30 + n));
        }
        V.record(s + 32, constant(40));
        V.record(s + 96, constant(41));
        assert_eq!((index_in(s + 32), index_in(s + 96)), (5, 6));
        V.sweep(true);
        assert_eq!(V.sweep(true).unwrap().freed, 2);
        let next: Vec<usize> = (8..11)
            .map(|n| {
                V.record(s + 32 * n, constant(50 + n));
                index_in(s + 32 * n)
            })
            .collect();
        assert_eq!(next, [1, 3, 7]);
        table::release(s as usize).unwrap();
    }

    #[test]
    fn the_change_log_gives_each_changed_index_once() {
        static V: Views = Views::new();
        let s = title_slots(4096);
        V.record(s, constant(60));
        V.record(s + 32, constant(61));
        let c = Consumer::of(&V);
        // every index at the first call
        assert_eq!(
            c.changes(),
            [(0, Some(constant(60))), (1, Some(constant(61)))]
        );
        assert!(c.changes().is_empty());
        let d = Consumer::of(&V);
        assert_eq!(d.changes().len(), 2);
        V.record(s + 64, constant(62));
        V.record(s + 64, constant(62));
        V.record(s + 32, constant(63));
        assert_eq!(
            c.changes(),
            [(2, Some(constant(62))), (3, Some(constant(63)))]
        );
        V.sweep(true);
        V.sweep(true);
        assert_eq!(c.changes(), [(1, None)]);
        // the other reads it all once, lowest first
        assert_eq!(
            d.changes(),
            [(1, None), (2, Some(constant(62))), (3, Some(constant(63)))]
        );
        assert!(V.reg().log.is_empty(), "trimmed once both read it");
        V.record(s + 96, constant(64));
        drop(d);
        assert_eq!(V.reg().log.len(), 1, "kept for the other");
        assert_eq!(c.changes(), [(1, Some(constant(64)))]);
        assert!(V.reg().log.is_empty());
        drop(c);
        V.record(s + 128, constant(65));
        assert!(V.reg().log.is_empty(), "no reader: nothing logged");
        // a reader that comes later reads every index
        let e = Consumer::of(&V);
        assert_eq!(e.changes().len(), 5);
        table::release(s as usize).unwrap();
    }

    #[test]
    fn slots_keep_their_views_under_records_copies_and_sweeps() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        static V: Views = Views::new();
        static STOP: AtomicBool = AtomicBool::new(false);
        const SLOTS: usize = 64;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let sweeper = std::thread::spawn(|| {
                let (mut n, mut freed) = (0, 0);
                while !STOP.load(SeqCst) {
                    freed += V.sweep(true).unwrap().freed;
                    n += 1;
                }
                (n, freed)
            });
            let workers: Vec<_> = (0..8u64)
                .map(|t| {
                    std::thread::spawn(move || {
                        // the first half a heap (placed in title memory), the rest outside slots
                        let base = title_slots(SLOTS * 32);
                        V.add_heap(Root {
                            start: base,
                            end: base + SLOTS as u64 * 16,
                            stride: 32,
                            owned: false,
                        });
                        let at = |k: usize| base + 32 * k as u64;
                        let read = |k: usize| {
                            // SAFETY: the thread's slot.
                            unsafe { tagged(at(k)) }.and_then(|i| V.view_at(i as usize))
                        };
                        let mut want: Vec<Option<View>> = vec![None; SLOTS];
                        let mut x = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(t + 1);
                        for _ in 0..40_000 {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            let k = (x % SLOTS as u64) as usize;
                            // copies go into heaps, as `CopyDescriptors` does
                            if x >> 40 & 3 == 0 && k < SLOTS / 2 {
                                let j = (x >> 20) as usize % SLOTS;
                                // SAFETY: the thread's slots.
                                unsafe {
                                    std::ptr::copy(at(j) as *const u8, at(k) as *mut u8, 32);
                                }
                                V.copied(at(k), 32, 32);
                                want[k] = want[j].clone();
                            } else {
                                // views shared by threads (dedup) and fresh ones
                                let v = View::Constant(
                                    0x7000_0000 + (x >> 24) % 4096 * 256,
                                    256 + (t % 2) as u32,
                                );
                                V.record(at(k), v.clone());
                                want[k] = Some(v);
                            }
                            assert_eq!(read(k), want[k]);
                        }
                        for (k, w) in want.iter().enumerate() {
                            assert_eq!(read(k), *w, "slot {k}");
                        }
                        base
                    })
                })
                .collect();
            let bases: Vec<u64> = workers.into_iter().map(|h| h.join().unwrap()).collect();
            STOP.store(true, SeqCst);
            let swept = sweeper.join().unwrap();
            tx.send((swept, V.reg().live)).unwrap();
            for b in bases {
                table::release(b as usize).unwrap();
            }
        });
        let ((sweeps, freed), live) = rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the stress took over 120 s");
        assert!(sweeps > 2 && freed > 0, "{sweeps} sweeps freed {freed}");
        assert!(live <= 8 * SLOTS + 2 * 8192, "{live} live");
    }
}
