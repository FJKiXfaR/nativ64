//! `api-ms-win-core-heap`: the process heap (`Heap*`, `Local*`).
//!
//! One heap exists, the process heap: [`GetProcessHeap`] returns a token that no other handle
//! uses, and every allocation is a block of the host allocator with a small header in front of
//! it. The header keeps the block's raw pointer and its requested size, so `HeapReAlloc` can
//! honour `HEAP_ZERO_MEMORY` on the grown tail and `HeapFree` releases the right pointer; the
//! returned block is 16-byte aligned, as Windows x64 requires. `LocalAlloc`/`LocalFree` use the
//! same blocks, so either pair can release them (as Windows' fixed local heap does).
//!
//! Flags: `HEAP_ZERO_MEMORY` (8) zeroes a fresh block and the new tail of a reallocation;
//! `HEAP_NO_SERIALIZE` (1), `HEAP_GENERATE_EXCEPTIONS` (4) and `HEAP_REALLOC_IN_PLACE_ONLY`
//! (16) are accepted and have no effect (the host allocator serialises itself, no exception is
//! raised, and a move is always allowed). `LocalAlloc` accepts 0 or `LMEM_ZEROINIT` (0x40) only.

use std::ffi::c_void;

const HEAP_NO_SERIALIZE: u32 = 0x1;
const HEAP_GENERATE_EXCEPTIONS: u32 = 0x4;
const HEAP_ZERO_MEMORY: u32 = 0x8;
const HEAP_REALLOC_IN_PLACE_ONLY: u32 = 0x10;
const LMEM_ZEROINIT: u32 = 0x40;
const KNOWN_HEAP_FLAGS: u32 =
    HEAP_NO_SERIALIZE | HEAP_GENERATE_EXCEPTIONS | HEAP_ZERO_MEMORY | HEAP_REALLOC_IN_PLACE_ONLY;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_FLAGS: u32 = 1004;

/// The process heap token: a static's address, so it is non-NULL, aligned and cannot collide with
/// a kernel handle (an index) or a window handle.
static PROCESS_HEAP: u8 = 0;

const MAGIC: usize = 0x6e36_3468_6561_7000; // "n64heap\0"
/// Header before every returned block: the raw malloc pointer, the requested size, the magic.
const HEADER: usize = 24;

fn fail(err: u32) -> *mut c_void {
    crate::api_ms_win_core_errorhandling::set_last_error(err);
    std::ptr::null_mut()
}

/// The block's header fields for a pointer this module returned.
///
/// # Safety
/// `p` is NULL or a pointer this module's allocations returned.
unsafe fn header(p: *mut c_void) -> Option<(usize, usize)> {
    let user = p as *mut u8;
    let magic = user.sub(8).cast::<usize>().read_unaligned();
    if magic != MAGIC {
        return None;
    }
    let size = user.sub(16).cast::<usize>().read_unaligned();
    let raw = user.sub(24).cast::<usize>().read_unaligned();
    Some((raw, size))
}

/// Bytes usable from a block's user pointer: the host block's capacity past the header.
///
/// # Safety
/// `p` is a pointer this module's allocations returned.
unsafe fn capacity(p: *mut c_void) -> usize {
    let Some((raw, _)) = header(p) else {
        return 0;
    };
    let used = p as usize - raw;
    (libc::malloc_usable_size(raw as *mut c_void) as usize).saturating_sub(used)
}

/// Allocates `size` bytes (16-byte aligned) and zeroes them when `zero`.
unsafe fn allocate(size: usize, zero: bool) -> *mut c_void {
    let Some(total) = size.checked_add(HEADER + 16) else {
        return fail(ERROR_NOT_ENOUGH_MEMORY);
    };
    let raw = libc::malloc(total) as usize;
    if raw == 0 {
        return fail(ERROR_NOT_ENOUGH_MEMORY);
    }
    let user = (raw + HEADER + 15) & !15;
    ((user - 24) as *mut usize).write_unaligned(raw);
    ((user - 16) as *mut usize).write_unaligned(size);
    ((user - 8) as *mut usize).write_unaligned(MAGIC);
    let p = user as *mut c_void;
    if zero {
        std::ptr::write_bytes(p, 0, size);
    }
    p
}

unsafe fn release(p: *mut c_void) -> bool {
    match header(p) {
        Some((raw, _)) => {
            libc::free(raw as *mut c_void);
            true
        }
        None => false,
    }
}

/// `HeapAlloc`/`LocalAlloc`: flags are checked, then a header block is malloc'ed.
pub(crate) unsafe fn heap_alloc(flags: u32, size: usize, known: u32) -> *mut c_void {
    if flags & !known != 0 {
        return fail(ERROR_INVALID_FLAGS);
    }
    allocate(
        size,
        flags & HEAP_ZERO_MEMORY != 0 || flags & LMEM_ZEROINIT != 0,
    )
}

unsafe fn heap_realloc(flags: u32, p: *mut c_void, size: usize) -> *mut c_void {
    if flags & !KNOWN_HEAP_FLAGS != 0 {
        return fail(ERROR_INVALID_FLAGS);
    }
    if p.is_null() {
        return allocate(size, flags & HEAP_ZERO_MEMORY != 0);
    }
    let Some((_, old)) = header(p) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let Some((raw, _)) = header(p) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let cap = capacity(p);
    if flags & HEAP_REALLOC_IN_PLACE_ONLY != 0 && size > cap {
        return fail(ERROR_NOT_ENOUGH_MEMORY);
    }
    let grow = size > old && flags & HEAP_ZERO_MEMORY != 0;
    // Grow in place when the host block has room, so a title's realloc-in-place idiom is cheap.
    if size <= cap {
        if grow {
            std::ptr::write_bytes((p as *mut u8).add(old), 0, size - old);
        }
        (p as *mut u8).sub(16).cast::<usize>().write_unaligned(size);
        return p;
    }
    let new = allocate(size, false);
    if new.is_null() {
        return new;
    }
    let copy = old.min(size);
    std::ptr::copy_nonoverlapping(p as *const u8, new as *mut u8, copy);
    if grow {
        std::ptr::write_bytes((new as *mut u8).add(copy), 0, size - copy);
    }
    libc::free(raw as *mut c_void);
    new
}

win32! {
    module "api-ms-win-core-heap" => "kernel32";
    Native fn GetProcessHeap() -> *mut c_void {
        &PROCESS_HEAP as *const u8 as *mut c_void
    }
    Native fn HeapAlloc(_heap: *mut c_void, flags: u32, size: usize) -> *mut c_void {
        unsafe { heap_alloc(flags, size, KNOWN_HEAP_FLAGS) }
    }
    Native fn HeapReAlloc(_heap: *mut c_void, flags: u32, p: *mut c_void, size: usize) -> *mut c_void {
        unsafe { heap_realloc(flags, p, size) }
    }
    Native fn HeapFree(_heap: *mut c_void, _flags: u32, p: *mut c_void) -> i32 {
        if p.is_null() {
            return 1;
        }
        if unsafe { release(p) } {
            return 1;
        }
        crate::api_ms_win_core_errorhandling::set_last_error(ERROR_INVALID_PARAMETER);
        0
    }
    // Fixed local allocations are heap blocks: LMEM_FIXED (0) or LPTR (LMEM_ZEROINIT).
    Native fn LocalAlloc(flags: u32, size: usize) -> *mut c_void {
        unsafe { heap_alloc(flags, size, LMEM_ZEROINIT) }
    }
    Native fn LocalFree(p: *mut c_void) -> *mut c_void {
        if !p.is_null() {
            unsafe {
                release(p);
            }
        }
        std::ptr::null_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;
    use crate::lookup;

    fn last_error() -> u32 {
        unsafe { GetLastError() }
    }

    #[test]
    fn exports_are_registered_under_kernel32() {
        for name in [
            "GetProcessHeap",
            "HeapAlloc",
            "HeapReAlloc",
            "HeapFree",
            "LocalAlloc",
            "LocalFree",
        ] {
            assert!(
                lookup("api-ms-win-core-heap-l2-1-0.dll", name).is_some(),
                "{name}"
            );
            assert!(lookup("kernel32.dll", name).is_some(), "{name}");
        }
    }

    #[test]
    fn process_heap_allocates_zeroes_and_frees() {
        unsafe {
            let heap = GetProcessHeap();
            assert!(!heap.is_null());
            let p = HeapAlloc(heap, HEAP_ZERO_MEMORY, 100);
            assert!(!p.is_null());
            assert_eq!(p as usize % 16, 0, "16-byte aligned");
            for i in 0..100 {
                assert_eq!(*(p as *const u8).add(i), 0, "zeroed byte {i}");
            }
            std::ptr::write_bytes(p, 0xab, 100);
            assert_eq!(HeapFree(heap, 0, p), 1);
            assert_eq!(HeapFree(heap, 0, std::ptr::null_mut()), 1);
        }
    }

    #[test]
    fn realloc_preserves_and_zeroes_the_new_tail() {
        unsafe {
            let heap = GetProcessHeap();
            let p = HeapAlloc(heap, HEAP_ZERO_MEMORY, 8);
            std::ptr::write_bytes(p, 0x11, 8);
            let p = HeapReAlloc(heap, HEAP_ZERO_MEMORY, p, 64);
            assert!(!p.is_null());
            for i in 0..8 {
                assert_eq!(*(p as *const u8).add(i), 0x11, "kept byte {i}");
            }
            for i in 8..64 {
                assert_eq!(*(p as *const u8).add(i), 0, "zeroed tail byte {i}");
            }
            let p = HeapReAlloc(heap, 0, p, 16);
            assert_eq!(HeapFree(heap, 0, p), 1);
        }
    }

    #[test]
    fn local_alloc_is_the_same_allocator() {
        unsafe {
            let p = LocalAlloc(LMEM_ZEROINIT, 32);
            assert!(!p.is_null());
            for i in 0..32 {
                assert_eq!(*(p as *const u8).add(i), 0);
            }
            // A Windows title may release a LocalAlloc block with HeapFree and vice versa.
            assert_eq!(HeapFree(GetProcessHeap(), 0, p), 1);
            let p = HeapAlloc(GetProcessHeap(), 0, 8);
            assert!(LocalFree(p).is_null());
            assert_eq!(LocalFree(std::ptr::null_mut()), std::ptr::null_mut());
        }
    }

    #[test]
    fn failures_set_the_last_error() {
        unsafe {
            let heap = GetProcessHeap();
            assert!(HeapAlloc(heap, 0x4000_0000, 16).is_null());
            assert_eq!(last_error(), ERROR_INVALID_FLAGS);
            assert!(HeapAlloc(heap, 0, usize::MAX).is_null());
            assert_eq!(last_error(), ERROR_NOT_ENOUGH_MEMORY);
            let p = HeapAlloc(heap, 0, 16);
            assert_eq!(HeapFree(heap, 0, (p as *mut u8).add(4).cast()), 0);
            assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
            assert_eq!(HeapFree(heap, 0, p), 1);
            assert_eq!(LocalAlloc(0x100, 16) as usize, 0);
            assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        }
    }
}
