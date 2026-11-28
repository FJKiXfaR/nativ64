//! Trap stubs: one tiny thunk per unresolved import, which reports the import and aborts.

use crate::LoadError;
use std::sync::Mutex;

const STUB_SIZE: usize = 32;

/// Names of every trapped import in the process, indexed by the number the stub passes.
static TRAPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

extern "win64" fn trap_handler(index: u32) -> ! {
    let name = match TRAPPED.try_lock() {
        Ok(g) => g.get(index as usize).cloned(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().get(index as usize).cloned(),
        Err(_) => None,
    };
    eprintln!(
        "nativ64: unimplemented import {}",
        name.as_deref().unwrap_or("<unknown>")
    );
    unsafe { libc::abort() }
}

/// An executable page holding the stubs of one image.
pub struct StubPage {
    base: *mut u8,
    len: usize,
    count: usize,
}

// The page is immutable (read+execute) once built.
unsafe impl Send for StubPage {}
unsafe impl Sync for StubPage {}

impl StubPage {
    /// Builds one stub per name; `addr(i)` gives the address of stub `i`.
    pub fn new(names: &[String]) -> Result<Self, LoadError> {
        let count = names.len();
        let len = (count.max(1) * STUB_SIZE).div_ceil(4096) * 4096;
        let first = {
            let mut t = TRAPPED.lock().unwrap_or_else(|p| p.into_inner());
            let first = t.len();
            t.extend(names.iter().cloned());
            first
        };
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(LoadError::Os("mmap(stubs)", errno()));
        }
        let base = base as *mut u8;
        for i in 0..count {
            // mov ecx, imm32 ; movabs rax, imm64 ; jmp rax
            let mut code = [0u8; STUB_SIZE];
            code[0] = 0xB9;
            code[1..5].copy_from_slice(&((first + i) as u32).to_le_bytes());
            code[5] = 0x48;
            code[6] = 0xB8;
            code[7..15].copy_from_slice(&(trap_handler as *const () as usize as u64).to_le_bytes());
            code[15] = 0xFF;
            code[16] = 0xE0;
            code[17..].fill(0xCC);
            unsafe {
                std::ptr::copy_nonoverlapping(code.as_ptr(), base.add(i * STUB_SIZE), STUB_SIZE)
            };
        }
        if unsafe { libc::mprotect(base.cast(), len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            let e = errno();
            unsafe { libc::munmap(base.cast(), len) };
            return Err(LoadError::Os("mprotect(stubs)", e));
        }
        Ok(StubPage { base, len, count })
    }

    pub fn addr(&self, i: usize) -> u64 {
        assert!(i < self.count);
        self.base as u64 + (i * STUB_SIZE) as u64
    }
}

impl Drop for StubPage {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Names of every traced import, indexed by the number its trampoline passes.
static TRACED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TRACE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// With `NATIV64_TRACE_TRIGGER`, the last trace lines are kept here instead of printed.
static TRACE_RING: Mutex<Option<std::collections::VecDeque<String>>> = Mutex::new(None);
const TRACE_RING_LINES: usize = 6000;

/// Starts keeping trace lines in a ring of the last [`TRACE_RING_LINES`] instead of printing them.
pub fn trace_ring_start() {
    *TRACE_RING.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(std::collections::VecDeque::with_capacity(TRACE_RING_LINES));
}

/// Prints the ring, then goes back to printing every trace line as it comes.
pub fn trace_ring_flush() {
    let ring = TRACE_RING.lock().unwrap_or_else(|p| p.into_inner()).take();
    for l in ring.into_iter().flatten() {
        eprintln!("{l}");
    }
}

fn trace_epoch() -> &'static std::time::Instant {
    static T: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T.get_or_init(std::time::Instant::now)
}

/// Number of traced import calls logged so far in the process.
pub fn trace_calls() -> u64 {
    TRACE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Substrings from `NATIV64_TRACE_FILTER` (comma-separated); `None` traces every import.
fn trace_filter() -> &'static Option<Vec<String>> {
    static FILTER: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    FILTER.get_or_init(|| {
        let v = std::env::var("NATIV64_TRACE_FILTER").ok()?;
        let subs: Vec<String> = v
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        (!subs.is_empty()).then_some(subs)
    })
}

/// Reads up to `buf.len()` bytes at `addr` of our own address space without faulting
/// (`process_vm_readv` reports EFAULT instead of raising SIGSEGV). Returns the byte count.
fn peek(addr: u64, buf: &mut [u8]) -> usize {
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    n.max(0) as usize
}

/// ` "text"` / ` u"text"` when the bytes at `addr` look like an ASCII or UTF-16LE string.
fn peek_string(addr: u64) -> String {
    if addr == 0 {
        return String::new();
    }
    let mut buf = [0u8; 96];
    let n = peek(addr, &mut buf);
    let buf = &buf[..n];
    let printable = |b: u8| (0x20..0x7F).contains(&b);
    let ascii: Vec<u8> = buf.iter().copied().take_while(|&b| printable(b)).collect();
    if ascii.len() >= 3 && (ascii.len() == buf.len() || buf[ascii.len()] == 0) {
        let more = if ascii.len() == buf.len() { "..." } else { "" };
        return format!(" \"{}{more}\"", String::from_utf8_lossy(&ascii));
    }
    let wide: Vec<u8> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .take_while(|c| c[1] == 0 && printable(c[0]))
        .map(|c| c[0])
        .collect();
    if wide.len() >= 3 {
        return format!(" u\"{}\"", String::from_utf8_lossy(&wide));
    }
    String::new()
}

/// `args` points at the guest's `[r9, r8, rdx, rcx]` saved by the trampoline.
extern "win64" fn trace_handler(index: u32, args: *const u64) {
    let name = match TRACED.try_lock() {
        Ok(g) => g.get(index as usize).cloned(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().get(index as usize).cloned(),
        Err(_) => None,
    };
    let name = name.as_deref().unwrap_or("<unknown>");
    if let Some(subs) = trace_filter() {
        if !subs.iter().any(|s| name.contains(s.as_str())) {
            return;
        }
    }
    TRACE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let [r9, r8, rdx, rcx] = unsafe { std::ptr::read(args.cast::<[u64; 4]>()) };
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let line = format!(
        "nativ64: [{tid}] {:.1}s call {name}({rcx:#x}, {rdx:#x}, {r8:#x}, {r9:#x}){}",
        trace_epoch().elapsed().as_secs_f32(),
        peek_string(rcx)
    );
    if let Ok(mut g) = TRACE_RING.lock() {
        if let Some(ring) = g.as_mut() {
            if ring.len() == TRACE_RING_LINES {
                ring.pop_front();
            }
            ring.push_back(line);
            return;
        }
    }
    eprintln!("{line}");
}

const TRAMPOLINE_SIZE: usize = 160;

/// An executable page of logging trampolines: each one saves the volatile argument registers,
/// logs the import, restores them and jumps to the real implementation (entry only).
pub struct TracePage {
    base: *mut u8,
    len: usize,
    count: usize,
}

unsafe impl Send for TracePage {}
unsafe impl Sync for TracePage {}

impl TracePage {
    /// One trampoline per `(name, target)`; `addr(i)` is the address of trampoline `i`.
    pub fn new(entries: &[(String, u64)]) -> Result<Self, LoadError> {
        let count = entries.len();
        let len = (count.max(1) * TRAMPOLINE_SIZE).div_ceil(4096) * 4096;
        let first = {
            let mut t = TRACED.lock().unwrap_or_else(|p| p.into_inner());
            let first = t.len();
            t.extend(entries.iter().map(|e| e.0.clone()));
            first
        };
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(LoadError::Os("mmap(trace)", errno()));
        }
        let base = base as *mut u8;
        for (i, (_, target)) in entries.iter().enumerate() {
            let mut c: Vec<u8> = Vec::with_capacity(TRAMPOLINE_SIZE);
            // push rcx; push rdx; push r8; push r9
            c.extend([0x51, 0x52, 0x41, 0x50, 0x41, 0x51]);
            // sub rsp, 0x68 (shadow space + 4 xmm + alignment)
            c.extend([0x48, 0x83, 0xEC, 0x68]);
            // movdqu [rsp+0x20+16*k], xmm_k
            for k in 0..4u8 {
                c.extend([0xF3, 0x0F, 0x7F, 0x44 | (k << 3), 0x24, 0x20 + 16 * k]);
            }
            // lea rdx, [rsp+0x68] (the pushed [r9, r8, rdx, rcx])
            c.extend([0x48, 0x8D, 0x54, 0x24, 0x68]);
            // mov ecx, index ; movabs rax, handler ; call rax
            c.push(0xB9);
            c.extend(((first + i) as u32).to_le_bytes());
            c.extend([0x48, 0xB8]);
            c.extend((trace_handler as *const () as usize as u64).to_le_bytes());
            c.extend([0xFF, 0xD0]);
            // movdqu xmm_k, [rsp+0x20+16*k]
            for k in 0..4u8 {
                c.extend([0xF3, 0x0F, 0x6F, 0x44 | (k << 3), 0x24, 0x20 + 16 * k]);
            }
            // add rsp, 0x68; pop r9; pop r8; pop rdx; pop rcx
            c.extend([0x48, 0x83, 0xC4, 0x68, 0x41, 0x59, 0x41, 0x58, 0x5A, 0x59]);
            // movabs r11, target ; jmp r11
            c.extend([0x49, 0xBB]);
            c.extend(target.to_le_bytes());
            c.extend([0x41, 0xFF, 0xE3]);
            assert!(c.len() <= TRAMPOLINE_SIZE);
            c.resize(TRAMPOLINE_SIZE, 0xCC);
            unsafe {
                std::ptr::copy_nonoverlapping(c.as_ptr(), base.add(i * TRAMPOLINE_SIZE), c.len())
            };
        }
        if unsafe { libc::mprotect(base.cast(), len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            let e = errno();
            unsafe { libc::munmap(base.cast(), len) };
            return Err(LoadError::Os("mprotect(trace)", e));
        }
        Ok(TracePage { base, len, count })
    }

    pub fn addr(&self, i: usize) -> u64 {
        assert!(i < self.count);
        self.base as u64 + (i * TRAMPOLINE_SIZE) as u64
    }
}

impl Drop for TracePage {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_string_reads_ascii_and_utf16_and_survives_bad_pointers() {
        let a = b"hello\0world";
        assert_eq!(peek_string(a.as_ptr() as u64), " \"hello\"");
        let w: Vec<u8> = "abc.bik\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(peek_string(w.as_ptr() as u64), " u\"abc.bik\"");
        assert_eq!(peek_string(0x10), "");
        assert_eq!(peek_string(0), "");
    }

    extern "win64" fn sum5(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
        a + 2 * b + 3 * c + 4 * d + 5 * e
    }
    extern "win64" fn scale(x: f64, y: f64, z: f64, w: f64) -> f64 {
        x + 2.0 * y + 3.0 * z + 4.0 * w
    }

    #[test]
    fn trampoline_preserves_args_and_stack() {
        let page = TracePage::new(&[
            ("t!sum5".into(), sum5 as *const () as usize as u64),
            ("t!scale".into(), scale as *const () as usize as u64),
        ])
        .unwrap();
        let before = trace_calls();
        let f: extern "win64" fn(u64, u64, u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(page.addr(0) as usize) };
        assert_eq!(f(1, 2, 3, 4, 5), 1 + 4 + 9 + 16 + 25);
        let g: extern "win64" fn(f64, f64, f64, f64) -> f64 =
            unsafe { std::mem::transmute(page.addr(1) as usize) };
        assert_eq!(g(1.0, 2.0, 3.0, 4.0), 1.0 + 4.0 + 9.0 + 16.0);
        assert!(trace_calls() >= before + 2);
    }
}
