//! Fake Thread and Process Environment Blocks, reachable through `gs` like on Windows.
//!
//! Windows code reads `gs:[0x30]` (the TEB itself), `gs:[0x58]` (static TLS array),
//! `gs:[0x60]` (the PEB) and so on. Rust and glibc on x86-64 Linux use `fs` for their own
//! thread pointer, so `gs` is free: we point it at a zeroed TEB per guest thread.
//!
//! Gap: threads created by nativ64 itself (`CreateThread`) do not get a TEB yet. When they
//! do, they must call [`install_for_current_thread`] (and set up static TLS) on the new
//! thread before running guest code.

use crate::stubs::errno;
use crate::LoadError;
use std::sync::OnceLock;

pub const TEB_SIZE: usize = 0x2000;
const PEB_SIZE: usize = 0x1000;

const ARCH_SET_GS: libc::c_long = 0x1001;

// TEB offsets (x64).
pub const TEB_EXCEPTION_LIST: usize = 0x00;
pub const TEB_STACK_BASE: usize = 0x08;
pub const TEB_STACK_LIMIT: usize = 0x10;
pub const TEB_SELF: usize = 0x30;
pub const TEB_PROCESS_ID: usize = 0x40;
pub const TEB_THREAD_ID: usize = 0x48;
pub const TEB_TLS_POINTER: usize = 0x58;
pub const TEB_PEB: usize = 0x60;
pub const TEB_LAST_ERROR: usize = 0x68;
// PEB offsets (x64).
pub const PEB_IMAGE_BASE: usize = 0x10;
pub const PEB_PROCESS_HEAP: usize = 0x30;

static PEB: OnceLock<usize> = OnceLock::new();

fn alloc_zeroed_pages(len: usize, what: &'static str) -> Result<*mut u8, LoadError> {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(LoadError::Os(what, errno()));
    }
    Ok(p.cast())
}

unsafe fn put(base: *mut u8, off: usize, v: usize) {
    base.add(off).cast::<usize>().write(v);
}

/// The process's PEB, allocated on first use (zeroed, then ProcessHeap filled in).
pub fn peb() -> Result<*mut u8, LoadError> {
    if let Some(&p) = PEB.get() {
        return Ok(p as *mut u8);
    }
    let p = alloc_zeroed_pages(PEB_SIZE, "mmap(PEB)")?;
    // SAFETY: `p` is a fresh writable page.
    unsafe { put(p, PEB_PROCESS_HEAP, process_heap()) };
    match PEB.set(p as usize) {
        Ok(()) => Ok(p),
        Err(_) => {
            // Lost a race: use the winner's block.
            unsafe { libc::munmap(p.cast(), PEB_SIZE) };
            Ok(PEB.get().copied().unwrap_or(0) as *mut u8)
        }
    }
}

/// Sets `PEB.ImageBaseAddress` (the main executable's base).
pub fn set_image_base(base: u64) -> Result<(), LoadError> {
    let p = peb()?;
    // SAFETY: the PEB page is ours and writable.
    unsafe { put(p, PEB_IMAGE_BASE, base as usize) };
    Ok(())
}

/// nativ64's process heap handle if it exports `GetProcessHeap`, else 0.
fn process_heap() -> usize {
    match nativ64::lookup("kernel32", "GetProcessHeap") {
        Some(e) => {
            // SAFETY: the export is a win64 function taking no argument.
            let f: extern "win64" fn() -> usize = unsafe { std::mem::transmute(e.addr) };
            f()
        }
        None => 0,
    }
}

/// A thread's TEB. Unmapped on drop (the thread must not touch `gs` afterwards).
pub struct Teb {
    ptr: *mut u8,
}

impl Teb {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Sets `TEB.ThreadLocalStoragePointer`.
    pub fn set_tls_pointer(&self, array: *mut usize) {
        // SAFETY: the TEB is ours and writable.
        unsafe { put(self.ptr, TEB_TLS_POINTER, array as usize) };
    }
}

impl Drop for Teb {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), TEB_SIZE) };
    }
}

/// Allocates a TEB for the calling thread and points `gs` at it.
/// `stack_base` is the high end of the stack, `stack_limit` the low end.
pub fn install_for_current_thread(stack_base: usize, stack_limit: usize) -> Result<Teb, LoadError> {
    let peb = peb()?;
    let ptr = alloc_zeroed_pages(TEB_SIZE, "mmap(TEB)")?;
    // SAFETY: `ptr` is a fresh writable block of TEB_SIZE bytes.
    unsafe {
        put(ptr, TEB_EXCEPTION_LIST, usize::MAX);
        put(ptr, TEB_STACK_BASE, stack_base);
        put(ptr, TEB_STACK_LIMIT, stack_limit);
        put(ptr, TEB_SELF, ptr as usize);
        put(ptr, TEB_PROCESS_ID, libc::getpid() as usize);
        put(ptr, TEB_THREAD_ID, libc::syscall(libc::SYS_gettid) as usize);
        put(ptr, TEB_PEB, peb as usize);
    }
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, ptr as usize) };
    if r != 0 {
        let e = errno();
        unsafe { libc::munmap(ptr.cast(), TEB_SIZE) };
        return Err(LoadError::Os("arch_prctl(ARCH_SET_GS)", e));
    }
    Ok(Teb { ptr })
}
