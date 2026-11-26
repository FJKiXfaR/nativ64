//! `api-ms-win-core-memory`: `VirtualAlloc`/`VirtualFree`/`VirtualProtect`/`VirtualQuery` on
//! `mmap`/`mprotect`/`munmap`, with a table of every allocation the title (and `xmem.dll`, which
//! allocates through [`alloc`]) made.
//!
//! * An allocation is a reservation: one anonymous `PROT_NONE` mapping placed at a 64 KiB
//!   boundary. The table keeps, per 4 KiB page, `0` (reserved) or the committed Windows
//!   protection. Committing is `mprotect`; decommitting maps fresh `PROT_NONE` pages over the
//!   range (so a later commit reads zeros); releasing unmaps the whole allocation.
//! * Errors follow Windows: `ERROR_INVALID_PARAMETER` (87) for a bad size, type or protection,
//!   `ERROR_INVALID_ADDRESS` (487) for an address outside the allocation it names, or free /
//!   uncommitted memory where committed memory is required, `ERROR_NOT_ENOUGH_MEMORY` (8) when no
//!   address range is left, `ERROR_BAD_LENGTH` (24) for a short `MEMORY_BASIC_INFORMATION`,
//!   `ERROR_INVALID_HANDLE` (6) for a process other than the current one (`-1`).
//! * Memory the table does not know (the loaded images, the host's own mappings) is read from
//!   `/proc/self/maps` by `VirtualQuery`/`VirtualProtect`: reported committed and private, its
//!   protection taken from the mapping's permissions.
//!
//! shortcut: `PAGE_GUARD` is recorded and reported but not enforced; `MEM_RESET`,
//! `MEM_WRITE_WATCH`, `MEM_PHYSICAL`, large pages and placeholders are refused as invalid types;
//! `MEM_TOP_DOWN` is accepted and ignored; `VirtualAlloc` addresses start at 1 TiB, out of the way
//! of graphics memory (4 GiB to 1 TiB).

use crate::api_ms_win_core_errorhandling::set_last_error;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_DECOMMIT: u32 = 0x4000;
pub const MEM_RELEASE: u32 = 0x8000;
pub const MEM_FREE: u32 = 0x1_0000;
pub const MEM_PRIVATE: u32 = 0x2_0000;
pub const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_TOP_DOWN: u32 = 0x10_0000;
/// With `MEM_RELEASE` and a size: gives the pages back but keeps the range reserved.
pub const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_BAD_LENGTH: u32 = 24;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NOACCESS: u32 = 998;
const ERROR_INVALID_ADDRESS: u32 = 487;

const PAGE: usize = 4096;
const GRANULARITY: usize = 64 << 10;
/// Highest user-mode address.
const MAX_ADDRESS: usize = 0x7FFF_FFFE_FFFF;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;
const PAGE_NOCACHE: u32 = 0x200;
const PAGE_WRITECOMBINE: u32 = 0x400;
/// `sizeof(MEMORY_BASIC_INFORMATION)` on x64.
const MBI_SIZE: usize = 48;
/// The process handle meaning "this process".
const CURRENT_PROCESS: usize = usize::MAX;

/// Where an allocation without an address may land: the first free aligned spot in `lo..=hi`.
#[derive(Clone, Copy)]
pub struct Placement {
    pub lo: usize,
    pub hi: usize,
    pub align: usize,
}

/// Default range of `VirtualAlloc` without an address.
const DEFAULT_PLACEMENT: Placement = Placement {
    lo: 1 << 40,
    hi: MAX_ADDRESS,
    align: GRANULARITY,
};

struct Region {
    size: usize,
    alloc_protect: u32,
    /// Per page: 0 reserved, else the committed Windows protection.
    pages: Vec<u16>,
}

struct Table {
    regions: BTreeMap<usize, Region>,
    /// Where the next address search of each `Placement::lo` starts.
    cursors: BTreeMap<usize, usize>,
}

fn table() -> MutexGuard<'static, Table> {
    static T: Mutex<Table> = Mutex::new(Table {
        regions: BTreeMap::new(),
        cursors: BTreeMap::new(),
    });
    T.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ranges the GPU backend must forget (see [`take_gpu_changes`]).
static CHANGES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
/// Whether anything watches [`CHANGES`]: nothing is recorded before the first `watch`.
static WATCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Starts recording the ranges that are decommitted, released, remapped or replaced, for
/// [`take_gpu_changes`]. Idempotent; the GPU backend calls it when it is made.
pub fn watch_gpu_changes() {
    WATCHED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Records that the memory at `addr..addr + len` changed under whatever the GPU imported
/// (a no-op until [`watch_gpu_changes`]). Also used by owners of memory outside the table.
pub fn note_change(addr: usize, len: usize) {
    if len != 0 && WATCHED.load(std::sync::atomic::Ordering::Relaxed) {
        CHANGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((addr, len));
    }
}

/// The `(address, length)` ranges recorded since the last call; empty (and cheap) mostly.
pub fn take_gpu_changes() -> Vec<(usize, usize)> {
    if !WATCHED.load(std::sync::atomic::Ordering::Relaxed) {
        return Vec::new();
    }
    std::mem::take(&mut *CHANGES.lock().unwrap_or_else(|e| e.into_inner()))
}

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// Windows protection to host protection bits.
fn host_prot(protect: u32) -> libc::c_int {
    match protect & 0xff {
        0x02 => libc::PROT_READ,
        0x04 | 0x08 => libc::PROT_READ | libc::PROT_WRITE,
        0x10 => libc::PROT_EXEC,
        0x20 => libc::PROT_READ | libc::PROT_EXEC,
        0x40 | 0x80 => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        _ => libc::PROT_NONE,
    }
}

/// Windows protection from host permissions (`rwx`).
fn win_prot(r: bool, w: bool, x: bool) -> u32 {
    match (r, w, x) {
        (false, false, false) => PAGE_NOACCESS,
        (true, false, false) => 0x02,
        (_, true, false) => 0x04,
        (false, false, true) => 0x10,
        (true, false, true) => 0x20,
        (_, true, true) => 0x40,
    }
}

/// A protection constant: one base value, plus at most the guard/no-cache/write-combine modifiers
/// (not on `PAGE_NOACCESS`, and not several at once).
fn valid_protect(p: u32) -> bool {
    let base = p & 0xff;
    let modifiers = p & !0xff;
    if !matches!(base, 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80) {
        return false;
    }
    if modifiers & !(PAGE_GUARD | PAGE_NOCACHE | PAGE_WRITECOMBINE) != 0
        || modifiers.count_ones() > 1
    {
        return false;
    }
    !(base == PAGE_NOACCESS && modifiers != 0)
}

fn mmap_fixed_reserve(at: usize, size: usize, flags: libc::c_int) -> bool {
    // SAFETY: an anonymous mapping; MAP_FIXED_NOREPLACE never replaces an existing one, and the
    // plain MAP_FIXED callers own the range through the table.
    let p = unsafe {
        libc::mmap(
            at as *mut c_void,
            size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | flags,
            -1,
            0,
        )
    };
    if p as usize == at {
        return true;
    }
    if p != libc::MAP_FAILED {
        // SAFETY: undoes the mapping a kernel without MAP_FIXED_NOREPLACE put elsewhere.
        unsafe { libc::munmap(p, size) };
    }
    false
}

/// Maps `size` inaccessible bytes at the first free aligned spot of `pl` after the range cursor.
fn place(t: &mut Table, size: usize, pl: Placement) -> Option<usize> {
    let align = pl.align.max(GRANULARITY);
    let step = align;
    let mut at = align_up(
        t.cursors.get(&pl.lo).copied().unwrap_or(0).max(pl.lo),
        align,
    );
    while at.checked_add(size).is_some_and(|e| e - 1 <= pl.hi) {
        if mmap_fixed_reserve(at, size, libc::MAP_FIXED_NOREPLACE) {
            t.cursors.insert(pl.lo, align_up(at + size, GRANULARITY));
            return Some(at);
        }
        at = align_up(at + step, align);
    }
    None
}

/// The allocation containing `addr`: its base.
fn find(t: &Table, addr: usize) -> Option<usize> {
    let (&base, r) = t.regions.range(..=addr).next_back()?;
    (addr < base + r.size).then_some(base)
}

/// Allocation core shared by `VirtualAlloc` and `XMemVirtualAlloc`. `kind` holds `MEM_RESERVE`
/// and/or `MEM_COMMIT` (`MEM_TOP_DOWN` is ignored); `placement` steers an allocation without an
/// address. Returns the address, or the Windows error code.
pub fn alloc(
    base: usize,
    size: usize,
    kind: u32,
    protect: u32,
    placement: Option<Placement>,
) -> Result<usize, u32> {
    let kind = kind & !MEM_TOP_DOWN;
    if kind & !(MEM_COMMIT | MEM_RESERVE) != 0 || kind == 0 || size == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    if size > MAX_ADDRESS || base > MAX_ADDRESS || base.checked_add(size).is_none() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let protect = if protect == 0 && kind & MEM_COMMIT == 0 {
        PAGE_NOACCESS
    } else {
        protect
    };
    if !valid_protect(protect) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let mut t = table();
    if base != 0 && kind & MEM_RESERVE == 0 {
        // Commit inside an existing reservation.
        let start = base & !(PAGE - 1);
        let end = align_up(base + size, PAGE);
        let r = find(&t, start).ok_or(ERROR_INVALID_ADDRESS)?;
        let region = t.regions.get_mut(&r).unwrap();
        if end > r + region.size {
            return Err(ERROR_INVALID_ADDRESS);
        }
        // SAFETY: the range lies in a mapping the table owns.
        if unsafe { libc::mprotect(start as *mut c_void, end - start, host_prot(protect)) } != 0 {
            return Err(ERROR_NOT_ENOUGH_MEMORY);
        }
        region.pages[(start - r) / PAGE..(end - r) / PAGE].fill(protect as u16);
        return Ok(start);
    }
    let (at, len) = if base != 0 {
        let at = base & !(GRANULARITY - 1);
        (at, align_up(base + size, PAGE) - at)
    } else {
        let len = align_up(size, PAGE);
        let at = place(&mut t, len, placement.unwrap_or(DEFAULT_PLACEMENT))
            .ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        (at, len)
    };
    if base != 0 {
        let overlaps = t
            .regions
            .range(..at + len)
            .next_back()
            .is_some_and(|(&b, r)| b + r.size > at);
        if overlaps || !mmap_fixed_reserve(at, len, libc::MAP_FIXED_NOREPLACE) {
            return Err(ERROR_INVALID_ADDRESS);
        }
    }
    let mut region = Region {
        size: len,
        alloc_protect: protect,
        pages: vec![0; len / PAGE],
    };
    if kind & MEM_COMMIT != 0 {
        // SAFETY: the mapping was just made.
        if unsafe { libc::mprotect(at as *mut c_void, len, host_prot(protect)) } != 0 {
            unsafe { libc::munmap(at as *mut c_void, len) };
            return Err(ERROR_NOT_ENOUGH_MEMORY);
        }
        region.pages.fill(protect as u16);
    }
    t.regions.insert(at, region);
    Ok(at)
}

/// `MEM_RESERVE | MEM_REPLACE_PLACEHOLDER`: turns the placeholder `base..base+size` (an
/// allocation of exactly that size, all pages reserved) into an ordinary reservation.
pub fn replace_placeholder(base: usize, size: usize, protect: u32) -> Result<usize, u32> {
    let mut t = table();
    let r = t.regions.get_mut(&base).ok_or(ERROR_INVALID_ADDRESS)?;
    if r.size != align_up(size, PAGE) || r.pages.iter().any(|&p| p != 0) {
        return Err(ERROR_INVALID_ADDRESS);
    }
    r.alloc_protect = if valid_protect(protect) {
        protect
    } else {
        PAGE_NOACCESS
    };
    drop(t);
    note_change(base, size);
    Ok(base)
}

/// Records that `addr..addr+len` (inside one allocation) now holds memory mapped over it by the
/// caller (`protect` pages, as `MEM_COMMIT` would), so `VirtualQuery` reports it committed.
pub fn mark_mapped(addr: usize, len: usize, protect: u32) -> Result<(), u32> {
    let mut t = table();
    let b = find(&t, addr).ok_or(ERROR_INVALID_ADDRESS)?;
    let region = t.regions.get_mut(&b).unwrap();
    let end = addr + len;
    if !addr.is_multiple_of(PAGE) || !len.is_multiple_of(PAGE) || end > b + region.size {
        return Err(ERROR_INVALID_ADDRESS);
    }
    region.pages[(addr - b) / PAGE..(end - b) / PAGE].fill(protect as u16);
    drop(t);
    note_change(addr, len);
    Ok(())
}

/// `MEM_RELEASE`: unmaps the allocation that starts exactly at `base`.
pub fn release(base: usize) -> Result<(), u32> {
    let mut t = table();
    let r = t.regions.remove(&base).ok_or(ERROR_INVALID_ADDRESS)?;
    // SAFETY: the whole mapping `alloc` made.
    unsafe { libc::munmap(base as *mut c_void, r.size) };
    drop(t);
    note_change(base, r.size);
    Ok(())
}

/// `VirtualFree` core.
pub fn free(addr: usize, size: usize, kind: u32) -> Result<(), u32> {
    match kind {
        MEM_RELEASE => {
            if size != 0 {
                return Err(ERROR_INVALID_PARAMETER);
            }
            release(addr)
        }
        _ if kind == MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER && size != 0 => placeholder(addr, size),
        MEM_DECOMMIT => {
            let mut t = table();
            let b = find(&t, addr).ok_or(ERROR_INVALID_ADDRESS)?;
            let (start, end) = if size == 0 {
                if addr != b {
                    return Err(ERROR_INVALID_PARAMETER);
                }
                (b, b + t.regions[&b].size)
            } else {
                let end = addr.checked_add(size).ok_or(ERROR_INVALID_PARAMETER)?;
                (addr & !(PAGE - 1), align_up(end, PAGE))
            };
            let region = t.regions.get_mut(&b).unwrap();
            if end > b + region.size {
                return Err(ERROR_INVALID_ADDRESS);
            }
            if !mmap_fixed_reserve(start, end - start, libc::MAP_FIXED) {
                return Err(ERROR_NOT_ENOUGH_MEMORY);
            }
            region.pages[(start - b) / PAGE..(end - b) / PAGE].fill(0);
            drop(t);
            note_change(start, end - start);
            Ok(())
        }
        _ => Err(ERROR_INVALID_PARAMETER),
    }
}

/// `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`: the range becomes an allocation of its own (a
/// placeholder: reserved pages, released later by its base) and the rest of the allocation it
/// was in is split around it.
fn placeholder(addr: usize, size: usize) -> Result<(), u32> {
    let mut t = table();
    let b = find(&t, addr).ok_or(ERROR_INVALID_ADDRESS)?;
    let end = addr.checked_add(size).ok_or(ERROR_INVALID_PARAMETER)?;
    if !addr.is_multiple_of(PAGE) || !end.is_multiple_of(PAGE) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    if end > b + t.regions[&b].size {
        return Err(ERROR_INVALID_ADDRESS);
    }
    if !mmap_fixed_reserve(addr, size, libc::MAP_FIXED) {
        return Err(ERROR_NOT_ENOUGH_MEMORY);
    }
    let mut head = t.regions.remove(&b).unwrap();
    let alloc_protect = head.alloc_protect;
    let mut tail = head.pages.split_off((end - b) / PAGE);
    head.pages.truncate((end - b) / PAGE);
    head.pages.truncate((addr - b) / PAGE);
    if !head.pages.is_empty() {
        head.size = head.pages.len() * PAGE;
        t.regions.insert(b, head);
    }
    t.regions.insert(
        addr,
        Region {
            size,
            alloc_protect,
            pages: vec![0; size / PAGE],
        },
    );
    if !tail.is_empty() {
        t.regions.insert(
            end,
            Region {
                size: tail.len() * PAGE,
                alloc_protect,
                pages: std::mem::take(&mut tail),
            },
        );
    }
    drop(t);
    note_change(addr, size);
    Ok(())
}

/// The run of committed, accessible pages of an allocation of the table around `addr`:
/// `start..end` (page aligned). None when `addr` is not in an allocation of the table or its
/// page is reserved or `PAGE_NOACCESS`. What the GPU backend imports.
pub fn committed_run(addr: usize) -> Option<(usize, usize)> {
    let t = table();
    let b = find(&t, addr & !(PAGE - 1))?;
    let r = &t.regions[&b];
    let usable = |p: u16| p != 0 && p & 0xff != PAGE_NOACCESS as u16;
    let i = (addr - b) / PAGE;
    if !usable(r.pages[i]) {
        return None;
    }
    let first = r.pages[..i]
        .iter()
        .rposition(|&p| !usable(p))
        .map_or(0, |k| k + 1);
    let last = r.pages[i..]
        .iter()
        .position(|&p| !usable(p))
        .map_or(r.pages.len(), |k| i + k);
    Some((b + first * PAGE, b + last * PAGE))
}

/// Whether every page of `addr..addr + size` is committed and accessible, in one allocation of
/// the table: the cost follows the size of the range, not of the allocation (unlike
/// [`committed_run`]).
pub fn is_committed(addr: usize, size: usize) -> bool {
    let Some(end) = addr.checked_add(size) else {
        return false;
    };
    let t = table();
    let Some(b) = find(&t, addr & !(PAGE - 1)) else {
        return false;
    };
    let r = &t.regions[&b];
    let (first, last) = ((addr - b) / PAGE, (end - b).div_ceil(PAGE));
    last <= r.pages.len()
        && r.pages[first..last]
            .iter()
            .all(|&p| p != 0 && p & 0xff != PAGE_NOACCESS as u16)
}

/// One host mapping from `/proc/self/maps`: start, end, readable, writable, executable.
fn host_mapping(addr: usize) -> Option<(usize, usize, bool, bool, bool)> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (s, e) = f.next()?.split_once('-')?;
        let (s, e) = (
            usize::from_str_radix(s, 16).ok()?,
            usize::from_str_radix(e, 16).ok()?,
        );
        let p = f.next()?.as_bytes();
        (s <= addr && addr < e).then(|| (s, e, p[0] == b'r', p[1] == b'w', p[2] == b'x'))
    })
}

/// The start of the next host mapping above `addr`, if any.
fn next_host_mapping(addr: usize) -> Option<usize> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .filter_map(|l| usize::from_str_radix(l.split('-').next()?, 16).ok())
        .filter(|&s| s > addr)
        .min()
}

/// `VirtualProtect` core: the previous protection of the first page.
pub fn protect(addr: usize, size: usize, new: u32) -> Result<u32, u32> {
    if size == 0 || !valid_protect(new) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let start = addr & !(PAGE - 1);
    let end = addr
        .checked_add(size)
        .map(|e| align_up(e, PAGE))
        .ok_or(ERROR_INVALID_PARAMETER)?;
    let mut t = table();
    if let Some(b) = find(&t, start) {
        let region = t.regions.get_mut(&b).unwrap();
        if end > b + region.size {
            return Err(ERROR_INVALID_ADDRESS);
        }
        let pages = &mut region.pages[(start - b) / PAGE..(end - b) / PAGE];
        if pages.contains(&0) {
            return Err(ERROR_INVALID_ADDRESS);
        }
        // SAFETY: the range lies in a mapping the table owns.
        if unsafe { libc::mprotect(start as *mut c_void, end - start, host_prot(new)) } != 0 {
            return Err(ERROR_INVALID_ADDRESS);
        }
        let old = pages[0] as u32;
        pages.fill(new as u16);
        return Ok(old);
    }
    let (_, e, r, w, x) = host_mapping(start).ok_or(ERROR_INVALID_ADDRESS)?;
    if end > e {
        return Err(ERROR_INVALID_ADDRESS);
    }
    // SAFETY: a mapping of this process; the caller asked for the change.
    if unsafe { libc::mprotect(start as *mut c_void, end - start, host_prot(new)) } != 0 {
        return Err(ERROR_INVALID_ADDRESS);
    }
    Ok(win_prot(r, w, x))
}

/// What `VirtualQuery` reports about an address (the fields of `MEMORY_BASIC_INFORMATION`).
#[derive(Debug, PartialEq, Eq)]
pub struct Info {
    pub base: usize,
    pub allocation_base: usize,
    pub allocation_protect: u32,
    pub size: usize,
    pub state: u32,
    pub protect: u32,
    pub kind: u32,
}

pub fn query(addr: usize) -> Result<Info, u32> {
    if addr > MAX_ADDRESS {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let page = addr & !(PAGE - 1);
    let t = table();
    if let Some(b) = find(&t, page) {
        let r = &t.regions[&b];
        let i = (page - b) / PAGE;
        let first = r.pages[i];
        let run = r.pages[i..].iter().take_while(|&&p| p == first).count();
        return Ok(Info {
            base: page,
            allocation_base: b,
            allocation_protect: r.alloc_protect,
            size: run * PAGE,
            state: if first == 0 { MEM_RESERVE } else { MEM_COMMIT },
            protect: first as u32,
            kind: MEM_PRIVATE,
        });
    }
    // Free as far as the next allocation of the table or mapping of the host.
    let next_table = t.regions.range(page..).next().map(|(&b, _)| b);
    let next_host = next_host_mapping(page);
    drop(t);
    if let Some((s, e, r, w, x)) = host_mapping(page) {
        return Ok(Info {
            base: page,
            allocation_base: s,
            allocation_protect: win_prot(r, w, x),
            size: e - page,
            state: MEM_COMMIT,
            protect: win_prot(r, w, x),
            kind: MEM_PRIVATE,
        });
    }
    let end = [next_table, next_host, Some(MAX_ADDRESS + 1)]
        .into_iter()
        .flatten()
        .min()
        .unwrap();
    Ok(Info {
        base: page,
        allocation_base: 0,
        allocation_protect: 0,
        size: end - page,
        state: MEM_FREE,
        protect: PAGE_NOACCESS,
        kind: 0,
    })
}

fn current(process: usize) -> bool {
    if process == CURRENT_PROCESS {
        true
    } else {
        set_last_error(ERROR_INVALID_HANDLE);
        false
    }
}

fn fail(e: u32) -> i32 {
    set_last_error(e);
    0
}

unsafe fn virtual_alloc(addr: usize, size: usize, kind: u32, protect: u32) -> *mut c_void {
    match alloc(addr, size, kind, protect, None) {
        Ok(a) => a as *mut c_void,
        Err(e) => {
            set_last_error(e);
            std::ptr::null_mut()
        }
    }
}

unsafe fn virtual_free(addr: usize, size: usize, kind: u32) -> i32 {
    match free(addr, size, kind) {
        Ok(()) => 1,
        Err(e) => fail(e),
    }
}

unsafe fn virtual_protect(addr: usize, size: usize, new: u32, old: *mut u32) -> i32 {
    if old.is_null() {
        return fail(ERROR_NOACCESS);
    }
    match protect(addr, size, new) {
        Ok(o) => {
            old.write_unaligned(o);
            1
        }
        Err(e) => fail(e),
    }
}

unsafe fn virtual_query(addr: usize, out: *mut u8, length: usize) -> usize {
    if length < MBI_SIZE {
        set_last_error(ERROR_BAD_LENGTH);
        return 0;
    }
    match query(addr) {
        Ok(i) => {
            let mut b = [0u8; MBI_SIZE];
            b[0..8].copy_from_slice(&i.base.to_le_bytes());
            b[8..16].copy_from_slice(&i.allocation_base.to_le_bytes());
            b[16..20].copy_from_slice(&i.allocation_protect.to_le_bytes());
            b[24..32].copy_from_slice(&i.size.to_le_bytes());
            b[32..36].copy_from_slice(&i.state.to_le_bytes());
            b[36..40].copy_from_slice(&i.protect.to_le_bytes());
            b[40..44].copy_from_slice(&i.kind.to_le_bytes());
            std::ptr::copy_nonoverlapping(b.as_ptr(), out, MBI_SIZE);
            MBI_SIZE
        }
        Err(e) => {
            set_last_error(e);
            0
        }
    }
}

crate::win32! {
    module "api-ms-win-core-memory" => "kernel32";
    Native fn VirtualAlloc(addr: usize, size: usize, kind: u32, protect: u32) -> *mut c_void {
        virtual_alloc(addr, size, kind, protect)
    }
    Native fn VirtualAllocEx(process: usize, addr: usize, size: usize, kind: u32, protect: u32) -> *mut c_void {
        if !current(process) {
            return std::ptr::null_mut();
        }
        virtual_alloc(addr, size, kind, protect)
    }
    Native fn VirtualFree(addr: usize, size: usize, kind: u32) -> i32 {
        virtual_free(addr, size, kind)
    }
    Native fn VirtualFreeEx(process: usize, addr: usize, size: usize, kind: u32) -> i32 {
        if !current(process) {
            return 0;
        }
        virtual_free(addr, size, kind)
    }
    Native fn VirtualProtect(addr: usize, size: usize, new: u32, old: *mut u32) -> i32 {
        virtual_protect(addr, size, new, old)
    }
    Native fn VirtualProtectEx(process: usize, addr: usize, size: usize, new: u32, old: *mut u32) -> i32 {
        if !current(process) {
            return 0;
        }
        virtual_protect(addr, size, new, old)
    }
    Native fn VirtualQuery(addr: usize, out: *mut u8, length: usize) -> usize {
        virtual_query(addr, out, length)
    }
    Native fn VirtualQueryEx(process: usize, addr: usize, out: *mut u8, length: usize) -> usize {
        if !current(process) {
            return 0;
        }
        virtual_query(addr, out, length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;

    const RW: u32 = 0x04;
    const RO: u32 = 0x02;

    fn err() -> u32 {
        unsafe { GetLastError() }
    }

    fn mbi(addr: usize) -> [u8; MBI_SIZE] {
        let mut b = [0u8; MBI_SIZE];
        assert_eq!(
            unsafe { VirtualQuery(addr, b.as_mut_ptr(), MBI_SIZE) },
            MBI_SIZE
        );
        b
    }

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn usize_at(b: &[u8], o: usize) -> usize {
        usize::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    #[test]
    fn reserve_commit_decommit_release() {
        unsafe {
            let p = VirtualAlloc(0, 0x10000, MEM_RESERVE, RW) as usize;
            assert_ne!(p, 0);
            assert_eq!(p % GRANULARITY, 0);
            let b = mbi(p);
            assert_eq!(usize_at(&b, 8), p);
            assert_eq!(usize_at(&b, 24), 0x10000);
            assert_eq!(u32_at(&b, 32), MEM_RESERVE);
            assert_eq!(u32_at(&b, 36), 0);
            assert_eq!(u32_at(&b, 40), MEM_PRIVATE);
            assert_eq!(VirtualAlloc(p, 0x1000, MEM_COMMIT, RW) as usize, p);
            (p as *mut u8).write(9);
            let b = mbi(p);
            assert_eq!(
                (usize_at(&b, 24), u32_at(&b, 32), u32_at(&b, 36)),
                (0x1000, MEM_COMMIT, RW)
            );
            let b = mbi(p + 0x1000);
            assert_eq!((usize_at(&b, 24), u32_at(&b, 32)), (0xf000, MEM_RESERVE));
            // decommit forgets the contents
            assert_eq!(VirtualFree(p, 0x1000, MEM_DECOMMIT), 1);
            assert_eq!(u32_at(&mbi(p), 32), MEM_RESERVE);
            assert_eq!(VirtualAlloc(p, 0x1000, MEM_COMMIT, RW) as usize, p);
            assert_eq!((p as *const u8).read(), 0);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
            assert_eq!(u32_at(&mbi(p), 32), MEM_FREE);
        }
    }

    #[test]
    fn alloc_errors() {
        unsafe {
            assert!(VirtualAlloc(0, 0, MEM_COMMIT, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, 0x1000, 0, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, 0x1000, MEM_COMMIT | 0x4, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, 0x1000, MEM_COMMIT, 0).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, 0x1000, MEM_COMMIT, RW | RO).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, 0x1000, MEM_COMMIT, PAGE_GUARD).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert!(VirtualAlloc(0, usize::MAX / 2, MEM_COMMIT, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            // commit outside any reservation
            assert!(VirtualAlloc(0x7000_0000_0000, 0x1000, MEM_COMMIT, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            // commit past the end of the reservation
            let p = VirtualAlloc(0, 0x10000, MEM_RESERVE, RW) as usize;
            assert!(VirtualAlloc(p + 0xf000, 0x2000, MEM_COMMIT, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            // reserving over an existing reservation
            assert!(VirtualAlloc(p, 0x1000, MEM_RESERVE, RW).is_null());
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            // reserve at an address: rounded down to the granularity
            let q = VirtualAlloc(0x6000_0001_3000, 0x1000, MEM_RESERVE | MEM_COMMIT, RW) as usize;
            assert_eq!(q, 0x6000_0001_0000);
            assert_eq!(VirtualFree(q, 0, MEM_RELEASE), 1);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn free_errors() {
        unsafe {
            let p = VirtualAlloc(0, 0x10000, MEM_RESERVE | MEM_COMMIT, RW) as usize;
            assert_eq!(VirtualFree(p, 0x10000, 0), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualFree(p, 0, MEM_FREE), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE | MEM_DECOMMIT), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualFree(p, 1, MEM_RELEASE), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            // release needs the allocation base
            assert_eq!(VirtualFree(p + 0x1000, 0, MEM_RELEASE), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            // decommit with size 0 needs the base too
            assert_eq!(VirtualFree(p + 0x1000, 0, MEM_DECOMMIT), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualFree(p + 0xf000, 0x2000, MEM_DECOMMIT), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            assert_eq!(VirtualFree(p, 0x10000, MEM_DECOMMIT), 1);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
            // double release, unknown address
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            // wrong process
            assert_eq!(VirtualFreeEx(0, p, 0, MEM_RELEASE), 0);
            assert_eq!(err(), ERROR_INVALID_HANDLE);
        }
    }

    #[test]
    fn committed_runs_and_recorded_changes() {
        watch_gpu_changes();
        let a = alloc(0, 0x10000, MEM_RESERVE, 0x04, None).unwrap();
        assert_eq!(committed_run(a), None);
        alloc(a + 0x1000, 0x3000, MEM_COMMIT, 0x04, None).unwrap();
        assert_eq!(committed_run(a + 0x2000), Some((a + 0x1000, a + 0x4000)));
        assert_eq!(committed_run(a), None);
        assert_eq!(committed_run(a + 0x4000), None);
        assert!(is_committed(a + 0x1800, 0x2000));
        assert!(!is_committed(a + 0x1800, 0x2900));
        assert!(!is_committed(a, 0x1000));
        free(a + 0x2000, 0x1000, MEM_DECOMMIT).unwrap();
        assert_eq!(committed_run(a + 0x1000), Some((a + 0x1000, a + 0x2000)));
        let ch = take_gpu_changes();
        assert!(ch.contains(&(a + 0x2000, 0x1000)), "{ch:x?}");
        release(a).unwrap();
        assert!(take_gpu_changes().contains(&(a, 0x10000)));
    }

    #[test]
    fn protect_and_errors() {
        unsafe {
            let p = VirtualAlloc(0, 0x10000, MEM_RESERVE, RW) as usize;
            let mut old = 0u32;
            // not committed
            assert_eq!(VirtualProtect(p, 0x1000, RO, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            assert_eq!(
                VirtualAlloc(p, 0x1000, MEM_COMMIT, PAGE_NOACCESS) as usize,
                p
            );
            assert_eq!(VirtualProtect(p, 0xfffc, RO, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            assert_eq!(VirtualProtect(p, 0x1000, RO, &mut old), 1);
            assert_eq!(old, PAGE_NOACCESS);
            assert_eq!(VirtualProtect(p, 0x1000, RW, &mut old), 1);
            assert_eq!(old, RO);
            (p as *mut u8).write(1);
            assert_eq!(u32_at(&mbi(p), 36), RW);
            assert_eq!(VirtualProtect(p, 0x1000, RW, std::ptr::null_mut()), 0);
            assert_eq!(err(), ERROR_NOACCESS);
            assert_eq!(VirtualProtect(p, 0, RW, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualProtect(p, 0x1000, 0, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualProtect(0x10000, 0x1000, RW, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_ADDRESS);
            assert_eq!(VirtualProtectEx(5, p, 0x1000, RW, &mut old), 0);
            assert_eq!(err(), ERROR_INVALID_HANDLE);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn query_errors_and_host_memory() {
        unsafe {
            let mut b = [0u8; MBI_SIZE];
            assert_eq!(VirtualQuery(0x1000, b.as_mut_ptr(), MBI_SIZE - 1), 0);
            assert_eq!(err(), ERROR_BAD_LENGTH);
            assert_eq!(VirtualQuery(usize::MAX, b.as_mut_ptr(), MBI_SIZE), 0);
            assert_eq!(err(), ERROR_INVALID_PARAMETER);
            assert_eq!(VirtualQueryEx(0, 0x1000, b.as_mut_ptr(), MBI_SIZE), 0);
            assert_eq!(err(), ERROR_INVALID_HANDLE);
            // page zero is free, with its extent
            let f = mbi(0);
            assert_eq!(u32_at(&f, 32), MEM_FREE);
            assert_eq!(u32_at(&f, 36), PAGE_NOACCESS);
            // a host mapping (this test's stack) reads as committed read-write
            let x = 5u8;
            let h = mbi(&x as *const u8 as usize);
            assert_eq!(u32_at(&h, 32), MEM_COMMIT);
            assert_eq!(u32_at(&h, 36), RW);
        }
    }

    #[test]
    fn placement_and_sharing_with_xmem_style_callers() {
        let pl = Placement {
            lo: 1 << 33,
            hi: (1 << 36) - 1,
            align: 2 << 20,
        };
        let p = alloc(0, 3 << 20, MEM_RESERVE | MEM_COMMIT, RW, Some(pl)).unwrap();
        assert!((1 << 33..1 << 36).contains(&p));
        assert_eq!(p % (2 << 20), 0);
        assert_eq!(query(p).unwrap().size, 3 << 20);
        // the generic API frees it
        assert_eq!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 1);
        assert!(release(p).is_err());
    }
}
