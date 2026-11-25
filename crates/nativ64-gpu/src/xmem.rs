//! `xmem.dll`: the console's memory manager. Graphics memory is ordinary host memory here (GPU
//! addresses are CPU addresses), so XMem flags only steer where an allocation lands.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
/// 2 MiB pages (alignment); with `PAGES_64K_BIT`, 64 KiB pages.
const PAGES_2M: u32 = 0x2000_0000;
const PAGES_64K_BIT: u32 = 0x40_0000;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// `XMemAlloc` attributes: alignment as log2 (5 bits at 24), bit 33 asks for a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
const PAGE: usize = 4096;
const MIB: usize = 1 << 20;
/// Host protection constants (the console adds GPU bits above 0x1ff, dropped here).
const HOST_PROTECTION: u32 = 0x1ff;
/// Our block header: the raw pointer and the size, just below the pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for 32-bit memory; else from
/// 4 GiB (kept for 32-bit requests) up to 46 down to 40 bits from the field at bit 12, or 40 bits
/// without one: the console keeps title memory there, and ray-tracing structures must be.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field as usize,
        _ => return (0x1000_0000, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

fn host_prot(protect: u32) -> libc::c_int {
    match protect & HOST_PROTECTION & !0x100 {
        0x01 | 0 => libc::PROT_NONE,
        0x02 => libc::PROT_READ,
        0x04 | 0x08 => libc::PROT_READ | libc::PROT_WRITE,
        0x10 => libc::PROT_EXEC,
        0x20 => libc::PROT_READ | libc::PROT_EXEC,
        _ => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
    }
}

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// Where the next search in each range class starts (0: 32-bit, 1: the rest).
static CURSOR: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// Maps `size` bytes inside `lo..=hi` at the first free aligned spot after the class cursor.
fn place(lo: usize, hi: usize, size: usize, align: usize, prot: libc::c_int) -> *mut c_void {
    let cursor = &CURSOR[usize::from(lo >= 1 << 32)];
    let step = align.max(64 << 10);
    let mut at = align_up(cursor.load(Ordering::Relaxed).max(lo), align);
    while at.checked_add(size).is_some_and(|e| e - 1 <= hi) {
        // SAFETY: a fresh anonymous mapping that never replaces an existing one.
        let p = unsafe {
            libc::mmap(
                at as *mut c_void,
                size,
                prot,
                libc::MAP_PRIVATE
                    | libc::MAP_ANONYMOUS
                    | libc::MAP_NORESERVE
                    | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if p as usize == at {
            cursor.store(at + size, Ordering::Relaxed);
            reservations().lock().unwrap().insert(at, size);
            return p;
        }
        if p != libc::MAP_FAILED {
            // SAFETY: undoes the mapping a kernel without MAP_FIXED_NOREPLACE put elsewhere.
            unsafe { libc::munmap(p, size) };
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            break;
        }
        at = align_up(at + step, align);
    }
    std::ptr::null_mut()
}

/// Start and size of what `place` mapped, to release it later.
fn reservations() -> &'static Mutex<std::collections::HashMap<usize, usize>> {
    static R: OnceLock<Mutex<std::collections::HashMap<usize, usize>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn virtual_alloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    if size == 0 {
        return std::ptr::null_mut();
    }
    let prot = host_prot(protect);
    let size = align_up(size, PAGE);
    if !base.is_null() {
        // Part of an earlier reservation: commit it, or reserve exactly there.
        if kind & MEM_RESERVE != 0 {
            let at = base as usize;
            // SAFETY: never replaces an existing mapping.
            let p = unsafe {
                libc::mmap(
                    base,
                    size,
                    if kind & MEM_COMMIT != 0 {
                        prot
                    } else {
                        libc::PROT_NONE
                    },
                    libc::MAP_PRIVATE
                        | libc::MAP_ANONYMOUS
                        | libc::MAP_NORESERVE
                        | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if p as usize != at {
                if p != libc::MAP_FAILED {
                    // SAFETY: undoes a stray mapping.
                    unsafe { libc::munmap(p, size) };
                }
                return std::ptr::null_mut();
            }
            reservations().lock().unwrap().insert(at, size);
            return base;
        }
        let start = base as usize & !(PAGE - 1);
        let len = align_up(base as usize + size - start, PAGE);
        // SAFETY: fails with ENOMEM on unmapped pages.
        let r = unsafe { libc::mprotect(start as *mut c_void, len, prot) };
        return if r == 0 { base } else { std::ptr::null_mut() };
    }
    let (lo, hi) = address_range(flags);
    let align = if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
        2 * MIB
    } else {
        64 << 10
    };
    // A reservation without MEM_COMMIT stays inaccessible until committed.
    let prot = if kind & MEM_COMMIT != 0 {
        prot
    } else {
        libc::PROT_NONE
    };
    place(lo, hi, size, align, prot)
}

nativ64::win32! {
    module "xmem";

    Native fn XMemVirtualAlloc(base: *mut c_void, size: usize, kind: u32, flags: u64, protect: u32) -> *mut c_void {
        virtual_alloc(base, size, kind, flags, protect)
    }

    Native fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
        if attributes & ALLOC_32BIT != 0 {
            return virtual_alloc(
                std::ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                3 | ADDRESS_32BIT,
                0x04,
            );
        }
        let align = (1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f)).max(16);
        let mut raw = std::ptr::null_mut();
        let Some(total) = size.checked_add(HEADER) else {
            return std::ptr::null_mut();
        };
        // SAFETY: the alignment is a power of two and a multiple of a pointer's size.
        if unsafe { libc::posix_memalign(&mut raw, align, total + align) } != 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: `raw` holds `total + align` bytes; the user pointer, `align` bytes in, keeps
        // the header below it inside the block.
        unsafe {
            let user = (raw as *mut u8).add(align);
            (user.sub(HEADER) as *mut [usize; 2]).write([raw as usize, size]);
            user as *mut c_void
        }
    }

    Native fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
        if address.is_null() {
            return;
        }
        if attributes & ALLOC_32BIT != 0 {
            if let Some(size) = reservations().lock().unwrap().remove(&(address as usize)) {
                // SAFETY: the whole mapping `place` made.
                unsafe { libc::munmap(address, size) };
            }
            return;
        }
        // SAFETY: the address came from `XMemAllocDefault`, which wrote the header.
        unsafe {
            let [raw, _] = (address as *const u8).sub(HEADER).cast::<[usize; 2]>().read();
            libc::free(raw as *mut c_void);
        }
    }

    // shortcut: the title's `__XMemAllocationHooks__` export is not consulted; XMemAlloc and
    // XMemFree go straight to the default allocator.
    Native fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
        XMemAllocDefault(size, attributes)
    }

    Native fn XMemFree(address: *mut c_void, attributes: u64) {
        XMemFreeDefault(address, attributes)
    }

    // A retail console has no tools memory.
    Native fn XMemIsToolsMemAvailable() -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RW: u32 = 0x04;

    #[test]
    fn graphics_memory_lands_at_or_above_4gib() {
        let p = unsafe {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * MIB,
                MEM_RESERVE | MEM_COMMIT,
                3,
                RW,
            )
        };
        assert!(!p.is_null());
        assert!(p as usize >= 1 << 32 && (p as usize) < 1 << 40);
        unsafe { (p as *mut u8).add(3 * MIB - 1).write(7) };
    }

    #[test]
    fn two_mib_pages_are_aligned() {
        let p = unsafe {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                MIB,
                MEM_RESERVE | MEM_COMMIT | PAGES_2M,
                3,
                RW,
            )
        };
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
    }

    #[test]
    fn reserve_then_commit() {
        let p = unsafe { XMemVirtualAlloc(std::ptr::null_mut(), 1 << 20, MEM_RESERVE, 3, RW) };
        assert!(!p.is_null());
        let q = unsafe { XMemVirtualAlloc(p, 4096, MEM_COMMIT, 3, RW) };
        assert_eq!(q, p);
        unsafe { (p as *mut u8).write(1) };
        assert!(unsafe { XMemVirtualAlloc(std::ptr::null_mut(), 0, MEM_RESERVE, 3, RW) }.is_null());
    }

    #[test]
    fn alloc_32bit_below_4gib_and_free() {
        let p = unsafe { XMemAllocDefault(64 << 10, ALLOC_32BIT) };
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        unsafe { (p as *mut u8).write(1) };
        unsafe { XMemFreeDefault(p, ALLOC_32BIT) };
    }

    #[test]
    fn alloc_honours_alignment() {
        let attrs = 12u64 << ALIGNMENT_SHIFT;
        let p = unsafe { XMemAlloc(100, attrs) };
        assert!(!p.is_null());
        assert_eq!(p as usize % 4096, 0);
        unsafe { XMemFree(p, attrs) };
        assert_eq!(unsafe { XMemIsToolsMemAvailable() }, 0);
    }
}
