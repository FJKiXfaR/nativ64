//! `AcpHal.dll`: the console's audio processor, seen from the title (see `docs/audio.md`).
//!
//! Three parts: the APU heap (host memory with 32-bit device addresses), the SHAPE context
//! arrays allocated from it, and the HAL object whose command and message channel completes
//! every command at once and ticks one audio-frame message per 128 samples at 48 kHz, worked
//! out from elapsed time when the title polls (no thread). The SHAPE mixer does not run.

use crate::com::{E_INVALIDARG, HRESULT, S_OK};
use std::alloc::Layout;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as i32;
pub const E_UNEXPECTED: HRESULT = 0x8000_FFFFu32 as i32;

/// One audio frame: 128 samples at 48 kHz.
pub const FRAME: Duration = Duration::from_micros(2667);
pub const MSG_AUDIO_FRAME: u32 = 2;
pub const MSG_COMMAND_DONE: u32 = 0x10;
pub const MSG_DISCONNECTED: u32 = 0x80;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------ the APU heap

/// Device addresses start here, so that 0 and small values never name memory.
const APU_BASE: u64 = 0x1000_0000;
const APU_LIMIT: u64 = 1 << 32;

struct Region {
    /// Host address.
    va: usize,
    size: u32,
    /// `Some` when `ApuAlloc` owns the memory, `None` for title memory mapped by address.
    layout: Option<Layout>,
}

#[derive(Default)]
struct Heap {
    /// By device address.
    regions: BTreeMap<u32, Region>,
}

static HEAP: Mutex<Heap> = Mutex::new(Heap {
    regions: BTreeMap::new(),
});

impl Heap {
    /// First fit in the 32-bit device space: a start aligned to `align`, plus `skew` bytes.
    fn find_gap(&self, size: u32, align: u64, skew: u64) -> Option<u32> {
        let need = size.max(1) as u64 + skew;
        let mut cursor = APU_BASE;
        for (&start, r) in &self.regions {
            let at = cursor.next_multiple_of(align);
            if at + need <= start as u64 {
                return Some((at + skew) as u32);
            }
            cursor = cursor.max(start as u64 + r.size.max(1) as u64);
        }
        let at = cursor.next_multiple_of(align);
        (at + need <= APU_LIMIT).then_some((at + skew) as u32)
    }

    /// The region holding a host address, with the offset in it.
    fn by_va(&self, va: usize) -> Option<(u32, &Region, u32)> {
        self.regions.iter().find_map(|(&apu, r)| {
            (va >= r.va && va - r.va < r.size.max(1) as usize).then(|| (apu, r, (va - r.va) as u32))
        })
    }

    fn by_apu(&self, apu: u32) -> Option<(&Region, u32)> {
        let (&start, r) = self.regions.range(..=apu).next_back()?;
        (apu - start < r.size.max(1)).then_some((r, apu - start))
    }
}

/// Zeroed host memory with a device address; `None` when either runs out.
fn heap_alloc(size: u32, align: u32) -> Option<(*mut u8, u32)> {
    let layout = Layout::from_size_align(size.max(1) as usize, (align as usize).max(16)).ok()?;
    let mut heap = lock(&HEAP);
    let apu = heap.find_gap(size, align as u64, 0)?;
    // SAFETY: the layout has a non-zero size.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        return None;
    }
    heap.regions.insert(
        apu,
        Region {
            va: ptr as usize,
            size,
            layout: Some(layout),
        },
    );
    Some((ptr, apu))
}

/// Removes the region that starts at `va`; false if there is none.
fn heap_free(va: usize) -> bool {
    let mut heap = lock(&HEAP);
    let Some((&apu, _)) = heap.regions.iter().find(|(_, r)| r.va == va) else {
        return false;
    };
    let region = heap.regions.remove(&apu).unwrap();
    if let Some(layout) = region.layout {
        // SAFETY: allocated by `heap_alloc` with this layout and removed from the map.
        unsafe { std::alloc::dealloc(va as *mut u8, layout) };
    }
    true
}

/// Writes `value` over the u32 at a device address if a region holds it.
fn write_word(apu: u32, value: u32) {
    let heap = lock(&HEAP);
    if let Some((r, off)) = heap.by_apu(apu) {
        if off as u64 + 4 <= r.size as u64 && (r.va + off as usize).is_multiple_of(4) {
            // SAFETY: four bytes inside a live region the title and the processor share.
            unsafe { ((r.va + off as usize) as *mut u32).write_volatile(value) };
        }
    }
}

// ------------------------------------------------------------------ SHAPE contexts

/// Bytes per context for rate conversion, EQ/compression, filter/volume, DMA, XMA and PCM:
/// the larger of what the two references use.
const CONTEXT_BYTES: [u32; 6] = [64, 256, 256, 64, 64, 256];

static SHAPE: Mutex<[usize; 6]> = Mutex::new([0; 6]);

fn free_arrays(arrays: [usize; 6]) {
    for a in arrays.into_iter().filter(|&a| a != 0) {
        heap_free(a);
    }
}

// ------------------------------------------------------------------ the HAL object

/// What `PopMessage` writes: 24 bytes, the part the two references share.
#[repr(C, packed)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Message {
    pub kind: u32,
    /// Messages lost to a full queue since the previous one.
    pub dropped: u32,
    /// Microseconds since the channel connected, truncated to 32 bits.
    pub usec: u32,
    /// The command a `MSG_COMMAND_DONE` answers.
    pub command: u32,
    pub id: u64,
}

/// The channel's state; `now` is passed in so tests control the clock.
#[derive(Default)]
pub struct Channel {
    messages: VecDeque<Message>,
    connected: bool,
    capacity: usize,
    dropped: u32,
    since: Option<Instant>,
    next_frame: Option<Instant>,
}

impl Channel {
    pub fn connect(&mut self, now: Instant, messages: u32) {
        *self = Channel {
            connected: true,
            capacity: messages as usize,
            since: Some(now),
            next_frame: Some(now + FRAME),
            ..Default::default()
        };
    }

    pub fn disconnect(&mut self, now: Instant) {
        let m = self.message(MSG_DISCONNECTED, 0, 0, now);
        self.messages.push_back(m);
        self.connected = false;
        self.next_frame = None;
    }

    fn message(&mut self, kind: u32, command: u32, id: u64, now: Instant) -> Message {
        let usec = self
            .since
            .map_or(0, |s| now.saturating_duration_since(s).as_micros());
        Message {
            kind,
            dropped: std::mem::take(&mut self.dropped),
            usec: usec as u32,
            command,
            id,
        }
    }

    /// A command completes at once; with an id it is acknowledged. False when not connected.
    pub fn submit(&mut self, command: u32, id: u64, now: Instant) -> bool {
        if !self.connected {
            return false;
        }
        if id != 0 {
            if self.capacity != 0 && self.messages.len() >= self.capacity {
                self.dropped = self.dropped.saturating_add(1);
            } else {
                let m = self.message(MSG_COMMAND_DONE, command, id, now);
                self.messages.push_back(m);
            }
        }
        true
    }

    fn frame_due(&self, now: Instant) -> bool {
        self.next_frame.is_some_and(|t| now >= t)
    }

    /// Queued messages first, then an audio frame if one is due. Frames more than 8 late are
    /// skipped rather than replayed.
    pub fn pop(&mut self, now: Instant) -> Option<Message> {
        if let Some(m) = self.messages.pop_front() {
            return Some(m);
        }
        if self.frame_due(now) {
            let next = self.next_frame? + FRAME;
            self.next_frame = Some(match now.checked_sub(FRAME * 8) {
                Some(floor) => next.max(floor),
                None => next,
            });
            return Some(self.message(MSG_AUDIO_FRAME, 0, 0, now));
        }
        None
    }

    pub fn count(&self, now: Instant) -> u32 {
        self.messages.len() as u32 + u32::from(self.frame_due(now))
    }
}

#[repr(C)]
struct HalVtbl {
    connect: unsafe extern "win64" fn(*mut Hal, u32, u32) -> HRESULT,
    disconnect: unsafe extern "win64" fn(*mut Hal) -> HRESULT,
    submit: unsafe extern "win64" fn(*mut Hal, u32, u64, u32, *const c_void, u32) -> HRESULT,
    pop_message: unsafe extern "win64" fn(*mut Hal, *mut Message) -> u8,
    message_count: unsafe extern "win64" fn(*mut Hal) -> u32,
    release: unsafe extern "win64" fn(*mut Hal),
}

/// The object `AcpHalCreate` returns: not a COM object, just a vtable of the six methods
/// above and no `IUnknown`.
#[repr(C)]
pub struct Hal {
    vtbl: &'static HalVtbl,
    channel: Mutex<Channel>,
}

static HAL_VTBL: HalVtbl = HalVtbl {
    connect: hal_connect,
    disconnect: hal_disconnect,
    submit: hal_submit,
    pop_message: hal_pop,
    message_count: hal_count,
    release: hal_release,
};

unsafe extern "win64" fn hal_connect(h: *mut Hal, _commands: u32, messages: u32) -> HRESULT {
    lock(&(*h).channel).connect(Instant::now(), messages);
    S_OK
}

unsafe extern "win64" fn hal_disconnect(h: *mut Hal) -> HRESULT {
    lock(&(*h).channel).disconnect(Instant::now());
    S_OK
}

// (command type, id, frame, data, notification): data is not interpreted. If the last argument
// is a device address inside a region, a one is stored there, as one reference does; the other
// reference calls it a size, which never lands in a region (device addresses start at 256 MB).
unsafe extern "win64" fn hal_submit(
    h: *mut Hal,
    command: u32,
    id: u64,
    _frame: u32,
    _data: *const c_void,
    notification: u32,
) -> HRESULT {
    if !lock(&(*h).channel).submit(command, id, Instant::now()) {
        return E_UNEXPECTED;
    }
    if notification as u64 >= APU_BASE {
        write_word(notification, 1);
    }
    S_OK
}

unsafe extern "win64" fn hal_pop(h: *mut Hal, out: *mut Message) -> u8 {
    match lock(&(*h).channel).pop(Instant::now()) {
        Some(m) => {
            if !out.is_null() {
                out.write_unaligned(m);
            }
            1
        }
        None => 0,
    }
}

unsafe extern "win64" fn hal_count(h: *mut Hal) -> u32 {
    lock(&(*h).channel).count(Instant::now())
}

unsafe extern "win64" fn hal_release(h: *mut Hal) {
    drop(Box::from_raw(h));
}

win32! {
    module "acphal";
    // (cached size, non-cached size): both references agree the first is non-zero. The sizes
    // are not enforced, and a second call succeeds (see docs/audio.md).
    Native fn ApuCreateHeap(cached: u32, _non_cached: u32) -> HRESULT {
        if cached == 0 { E_INVALIDARG } else { S_OK }
    }
    // (&va, &apu, size, alignment, flags): zeroed memory and its device address.
    Native fn ApuAlloc(va: *mut *mut c_void, apu: *mut u32, size: u32, align: u32,
                       _flags: u32) -> HRESULT {
        if va.is_null() || size == 0 || align < 4 || !align.is_power_of_two() {
            return E_INVALIDARG;
        }
        let Some((ptr, address)) = heap_alloc(size, align) else {
            return E_OUTOFMEMORY;
        };
        va.write(ptr.cast());
        if !apu.is_null() {
            apu.write(address);
        }
        S_OK
    }
    // (va): frees an ApuAlloc block or drops a mapping of title memory.
    Native fn ApuFree(va: *mut c_void) -> HRESULT {
        if va.is_null() || !heap_free(va as usize) { E_INVALIDARG } else { S_OK }
    }
    // (&apu, va, size): a device address for memory the title owns; inside a known block it is
    // that block's address plus the offset.
    Native fn ApuMapVirtualAddress(apu: *mut u32, va: *mut c_void, size: u32) -> HRESULT {
        if apu.is_null() || va.is_null() || size == 0 {
            return E_INVALIDARG;
        }
        let mut heap = lock(&HEAP);
        if let Some((start, r, off)) = heap.by_va(va as usize) {
            if off as u64 + size as u64 > r.size as u64 {
                return E_INVALIDARG;
            }
            apu.write(start + off);
            return S_OK;
        }
        let skew = va as u64 & 0xFFF;
        let Some(address) = heap.find_gap(size, 0x1000, skew) else {
            return E_OUTOFMEMORY;
        };
        heap.regions.insert(address, Region { va: va as usize, size, layout: None });
        apu.write(address);
        S_OK
    }
    // (apu): the host memory behind a device address, NULL outside every region.
    Native fn ApuMapApuAddress(apu: u32) -> *mut c_void {
        let heap = lock(&HEAP);
        match heap.by_apu(apu) {
            Some((r, off)) => (r.va + off as usize) as *mut c_void,
            None => std::ptr::null_mut(),
        }
    }
    // (va, size or alignment: unused): whether a device address exists for the memory.
    Native fn ApuIsVirtualAddressValid(va: *const c_void, _size: u32) -> i32 {
        i32::from(lock(&HEAP).by_va(va as usize).is_some())
    }
    // (&request): six counts (the sixth is a reserved word in one reference), then six host
    // pointers from offset 24, then six device addresses from offset 72. The sixth pointer and
    // the device addresses are written only when the sixth count is not zero, so a smaller
    // structure is never overrun. A second call replaces the first arrays.
    Native fn AcpHalAllocateShapeContexts(request: *mut u8) -> HRESULT {
        if request.is_null() {
            return E_INVALIDARG;
        }
        let counts: [u32; 6] = std::ptr::read_unaligned(request.cast());
        let mut arrays = [0usize; 6];
        let mut devices = [0u32; 6];
        for i in 0..6 {
            if counts[i] == 0 {
                continue;
            }
            let bytes = counts[i].checked_mul(CONTEXT_BYTES[i]);
            let Some((ptr, apu)) = bytes.and_then(|b| heap_alloc(b, 256)) else {
                free_arrays(arrays);
                return E_OUTOFMEMORY;
            };
            arrays[i] = ptr as usize;
            devices[i] = apu;
        }
        free_arrays(std::mem::replace(&mut *lock(&SHAPE), arrays));
        let pointers = if counts[5] != 0 { 6 } else { 5 };
        for (i, &a) in arrays.iter().enumerate().take(pointers) {
            request.add(24 + 8 * i).cast::<usize>().write_unaligned(a);
        }
        if counts[5] != 0 {
            request.add(72).cast::<[u32; 6]>().write_unaligned(devices);
        }
        S_OK
    }
    Native fn AcpHalReleaseShapeContexts() -> HRESULT {
        free_arrays(std::mem::take(&mut *lock(&SHAPE)));
        S_OK
    }
    // (&hal)
    Native fn AcpHalCreate(out: *mut *mut Hal) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        out.write(Box::into_raw(Box::new(Hal {
            vtbl: &HAL_VTBL,
            channel: Mutex::new(Channel::default()),
        })));
        S_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;
    use std::ptr::{null, null_mut};

    /// The APU heap is process-global: tests that touch it run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    unsafe fn alloc(size: u32, align: u32) -> (*mut c_void, u32) {
        let (mut va, mut apu) = (null_mut(), 0);
        assert_eq!(ApuAlloc(&mut va, &mut apu, size, align, 0), S_OK);
        (va, apu)
    }

    #[test]
    fn exports_resolve_by_file_name() {
        let _guard = serial();
        for n in [
            "AcpHalAllocateShapeContexts",
            "AcpHalCreate",
            "AcpHalReleaseShapeContexts",
            "ApuAlloc",
            "ApuCreateHeap",
            "ApuFree",
            "ApuIsVirtualAddressValid",
            "ApuMapApuAddress",
            "ApuMapVirtualAddress",
        ] {
            assert!(lookup("AcpHal.dll", n).is_some(), "{n}");
        }
    }

    #[test]
    fn create_heap_needs_a_size_and_repeats() {
        let _guard = serial();
        unsafe {
            assert_eq!(ApuCreateHeap(0, 0x100), E_INVALIDARG);
            assert_eq!(ApuCreateHeap(0x1000, 0), S_OK);
            assert_eq!(ApuCreateHeap(0x1000, 0), S_OK);
        }
    }

    #[test]
    fn alloc_aligns_zeroes_and_maps_both_ways() {
        let _guard = serial();
        unsafe {
            let (va, apu) = alloc(0x5780, 0x800);
            assert_eq!(apu % 0x800, 0);
            assert!(apu as u64 >= APU_BASE);
            assert_eq!(va as usize % 0x800, 0);
            assert!(std::slice::from_raw_parts(va.cast::<u8>(), 0x5780)
                .iter()
                .all(|&b| b == 0));
            assert_eq!(ApuMapApuAddress(apu), va);
            assert_eq!(ApuMapApuAddress(apu + 0x10), va.byte_add(0x10));
            assert_eq!(ApuMapApuAddress(apu + 0x5780), null_mut());
            assert_eq!(ApuMapApuAddress(0), null_mut());
            let mut back = 0;
            assert_eq!(ApuMapVirtualAddress(&mut back, va.byte_add(0x20), 16), S_OK);
            assert_eq!(back, apu + 0x20);
            assert_eq!(ApuIsVirtualAddressValid(va.byte_add(0x5000), 4), 1);
            assert_eq!(ApuIsVirtualAddressValid(null(), 4), 0);
            assert_eq!(ApuFree(va), S_OK);
            assert_eq!(ApuMapApuAddress(apu), null_mut());
            assert_eq!(ApuIsVirtualAddressValid(va, 4), 0);
            assert_eq!(ApuFree(va), E_INVALIDARG);
        }
    }

    #[test]
    fn alloc_rejects_bad_arguments() {
        let _guard = serial();
        unsafe {
            let (mut va, mut apu): (*mut c_void, u32) = (null_mut(), 0);
            assert_eq!(ApuAlloc(null_mut(), &mut apu, 16, 16, 0), E_INVALIDARG);
            assert_eq!(ApuAlloc(&mut va, &mut apu, 0, 16, 0), E_INVALIDARG);
            assert_eq!(ApuAlloc(&mut va, &mut apu, 16, 0, 0), E_INVALIDARG);
            assert_eq!(ApuAlloc(&mut va, &mut apu, 16, 2, 0), E_INVALIDARG);
            assert_eq!(ApuAlloc(&mut va, &mut apu, 16, 24, 0), E_INVALIDARG);
            assert!(va.is_null());
            // the device address is optional
            assert_eq!(ApuAlloc(&mut va, null_mut(), 16, 16, 0), S_OK);
            assert_eq!(ApuFree(va), S_OK);
            // more than the 32-bit device space holds
            assert_eq!(ApuAlloc(&mut va, &mut apu, u32::MAX, 16, 0), E_OUTOFMEMORY);
            assert_eq!(ApuFree(null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn blocks_do_not_overlap_and_addresses_are_reused() {
        let _guard = serial();
        unsafe {
            let (a, apu_a) = alloc(0x1000, 0x1000);
            let (b, apu_b) = alloc(0x1000, 0x1000);
            assert!(apu_b >= apu_a + 0x1000 || apu_a >= apu_b + 0x1000);
            assert_eq!(ApuFree(a), S_OK);
            let (c, apu_c) = alloc(0x800, 0x800);
            assert_eq!(apu_c, apu_a);
            assert_eq!(ApuFree(b), S_OK);
            assert_eq!(ApuFree(c), S_OK);
        }
    }

    #[test]
    fn title_memory_maps_to_a_device_address_and_back() {
        let _guard = serial();
        unsafe {
            let mut mine = vec![0u8; 0x3000];
            let va = mine.as_mut_ptr().add(0x123).cast::<c_void>();
            let mut apu = 0;
            assert_eq!(ApuMapVirtualAddress(&mut apu, va, 0x100), S_OK);
            assert_eq!(apu as usize & 0xFFF, va as usize & 0xFFF);
            assert_eq!(ApuMapApuAddress(apu), va);
            assert_eq!(ApuMapApuAddress(apu + 0x20), va.byte_add(0x20));
            assert_eq!(ApuIsVirtualAddressValid(va, 4), 1);
            // inside the mapping: the same address; past its end: refused
            let mut again = 0;
            assert_eq!(ApuMapVirtualAddress(&mut again, va.byte_add(8), 8), S_OK);
            assert_eq!(again, apu + 8);
            assert_eq!(
                ApuMapVirtualAddress(&mut again, va.byte_add(8), 0x1000),
                E_INVALIDARG
            );
            assert_eq!(ApuFree(va), S_OK);
            assert_eq!(ApuMapApuAddress(apu), null_mut());
            assert_eq!(ApuMapVirtualAddress(null_mut(), va, 16), E_INVALIDARG);
            assert_eq!(
                ApuMapVirtualAddress(&mut again, null_mut(), 16),
                E_INVALIDARG
            );
            assert_eq!(ApuMapVirtualAddress(&mut again, va, 0), E_INVALIDARG);
        }
    }

    #[test]
    fn shape_contexts_are_allocated_replaced_and_released() {
        let _guard = serial();
        unsafe {
            assert_eq!(AcpHalAllocateShapeContexts(null_mut()), E_INVALIDARG);
            let mut req = [0u8; 96];
            req[..20].copy_from_slice(
                &[8u32, 0, 4, 0, 16]
                    .iter()
                    .flat_map(|c| c.to_le_bytes())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(AcpHalAllocateShapeContexts(req.as_mut_ptr()), S_OK);
            fn ptr(req: &[u8; 96], i: usize) -> usize {
                usize::from_le_bytes(req[24 + 8 * i..32 + 8 * i].try_into().unwrap())
            }
            assert_ne!(ptr(&req, 0), 0);
            assert_eq!(ptr(&req, 1), 0);
            assert_ne!(ptr(&req, 2), 0);
            assert_ne!(ptr(&req, 4), 0);
            // the sixth count is zero: nothing written past the fifth pointer
            assert!(req[64..].iter().all(|&b| b == 0));
            let first = ptr(&req, 4);
            // the array is zeroed host memory of 16 contexts
            assert!(std::slice::from_raw_parts(first as *const u8, 16 * 64)
                .iter()
                .all(|&b| b == 0));
            // a second request replaces the first arrays
            assert_eq!(AcpHalAllocateShapeContexts(req.as_mut_ptr()), S_OK);
            assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
            assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
            // a sixth count writes the sixth pointer and the device addresses
            req[20..24].copy_from_slice(&2u32.to_le_bytes());
            assert_eq!(AcpHalAllocateShapeContexts(req.as_mut_ptr()), S_OK);
            let apu0 = req.as_ptr().add(72).cast::<u32>().read_unaligned();
            assert_eq!(ApuMapApuAddress(apu0) as usize, ptr(&req, 0));
            // a count that cannot be allocated fails and leaves the previous arrays
            req[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert_eq!(AcpHalAllocateShapeContexts(req.as_mut_ptr()), E_OUTOFMEMORY);
            assert_eq!(AcpHalReleaseShapeContexts(), S_OK);
        }
    }

    unsafe fn pop(h: *mut Hal) -> Option<Message> {
        let mut m = Message::default();
        ((*h).vtbl.pop_message)(h, &mut m).ne(&0).then_some(m)
    }

    #[test]
    fn hal_acknowledges_commands_and_ticks_frames() {
        let _guard = serial();
        unsafe {
            assert_eq!(AcpHalCreate(null_mut()), E_INVALIDARG);
            let mut h = null_mut();
            assert_eq!(AcpHalCreate(&mut h), S_OK);
            let v = (*h).vtbl;
            // nothing before Connect
            assert_eq!((v.submit)(h, 2, 7, 0, null(), 0), E_UNEXPECTED);
            assert_eq!((v.message_count)(h), 0);
            assert_eq!((v.connect)(h, 64, 64), S_OK);
            assert_eq!((v.submit)(h, 2, 0xF0F0_F0F0, 0, null(), 0), S_OK);
            // an id of 0 is not acknowledged
            assert_eq!((v.submit)(h, 9, 0, 0, null(), 0), S_OK);
            assert_eq!((v.message_count)(h), 1);
            let m = pop(h).unwrap();
            assert_eq!({ m.kind }, MSG_COMMAND_DONE);
            assert_eq!({ m.id }, 0xF0F0_F0F0);
            assert_eq!({ m.command }, 2);
            assert!(pop(h).is_none());
            // a frame is not due yet, and is after a while
            std::thread::sleep(FRAME * 3);
            assert_eq!((v.message_count)(h), 1);
            assert_eq!({ pop(h).unwrap().kind }, MSG_AUDIO_FRAME);
            assert_eq!((v.disconnect)(h), S_OK);
            assert_eq!({ pop(h).unwrap().kind }, MSG_DISCONNECTED);
            assert!(pop(h).is_none());
            assert_eq!((v.submit)(h, 2, 7, 0, null(), 0), E_UNEXPECTED);
            assert_eq!((v.pop_message)(h, null_mut()), 0);
            (v.release)(h);
        }
    }

    #[test]
    fn channel_frames_follow_the_clock() {
        let _guard = serial();
        let t0 = Instant::now();
        let mut ch = Channel::default();
        ch.connect(t0, 4);
        assert_eq!(ch.count(t0), 0);
        assert!(ch.pop(t0 + FRAME / 2).is_none());
        let m = ch.pop(t0 + FRAME).unwrap();
        assert_eq!({ m.kind }, MSG_AUDIO_FRAME);
        assert_eq!({ m.usec }, FRAME.as_micros() as u32);
        assert!(ch.pop(t0 + FRAME).is_none());
        assert!(ch.pop(t0 + FRAME * 2).is_some());
        // a title that stops polling gets at most 10 frames back, not thousands
        let late = t0 + Duration::from_secs(10);
        let n = std::iter::from_fn(|| ch.pop(late)).count();
        assert!((8..=10).contains(&n), "{n}");
    }

    #[test]
    fn channel_queue_is_bounded_and_counts_drops() {
        let _guard = serial();
        let t0 = Instant::now();
        let mut ch = Channel::default();
        ch.connect(t0, 2);
        for id in 1..=4 {
            assert!(ch.submit(5, id, t0));
        }
        assert_eq!({ ch.pop(t0).unwrap().id }, 1);
        assert_eq!({ ch.pop(t0).unwrap().id }, 2);
        assert!(ch.pop(t0).is_none());
        assert!(ch.submit(5, 9, t0));
        assert_eq!({ ch.pop(t0).unwrap().dropped }, 2);
        // a reconnect forgets what was queued
        ch.submit(5, 10, t0);
        ch.connect(t0, 2);
        assert!(ch.pop(t0).is_none());
    }

    #[test]
    fn notification_word_is_set_when_it_is_a_device_address() {
        let _guard = serial();
        unsafe {
            let (va, apu) = alloc(0x100, 0x100);
            let mut h = null_mut();
            assert_eq!(AcpHalCreate(&mut h), S_OK);
            let v = (*h).vtbl;
            assert_eq!((v.connect)(h, 8, 8), S_OK);
            assert_eq!((v.submit)(h, 1, 0, 0, null(), apu + 8), S_OK);
            assert_eq!(va.byte_add(8).cast::<u32>().read(), 1);
            // a small value (a size) is left alone
            assert_eq!((v.submit)(h, 1, 0, 0, null(), 0x40), S_OK);
            (v.release)(h);
            assert_eq!(ApuFree(va), S_OK);
        }
    }
}
