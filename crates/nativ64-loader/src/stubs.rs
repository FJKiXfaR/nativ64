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

/// Number of traced import calls logged so far in the process.
pub fn trace_calls() -> u64 {
    TRACE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The imports called most recently, newest last: what a crash report shows (`--trace-imports`
/// keeps this even when `NATIV64_TRACE_SILENT` suppresses the per-call lines).
static RECENT: std::sync::Mutex<std::collections::VecDeque<u32>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const RECENT_MAX: usize = 64;

/// The last imports called (their names, newest last).
pub fn recent_imports() -> Vec<String> {
    let recent = RECENT.lock().unwrap_or_else(|p| p.into_inner());
    let traced = TRACED.lock().unwrap_or_else(|p| p.into_inner());
    recent
        .iter()
        .map(|&i| {
            traced
                .get(i as usize)
                .cloned()
                .unwrap_or_else(|| format!("<trace {i}>"))
        })
        .collect()
}

/// True when the per-call lines are wanted (`--trace-imports` without `NATIV64_TRACE_SILENT`).
fn trace_prints() -> bool {
    !std::env::var_os("NATIV64_TRACE_SILENT").is_some_and(|v| !v.is_empty() && v != "0")
}

extern "win64" fn trace_handler(index: u32) {
    TRACE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut recent) = RECENT.try_lock() {
        if recent.len() == RECENT_MAX {
            recent.pop_front();
        }
        recent.push_back(index);
    }
    if trace_prints() {
        let name = match TRACED.try_lock() {
            Ok(g) => g.get(index as usize).cloned(),
            Err(std::sync::TryLockError::Poisoned(p)) => {
                p.into_inner().get(index as usize).cloned()
            }
            Err(_) => None,
        };
        eprintln!("nativ64: call {}", name.as_deref().unwrap_or("<unknown>"));
    }
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

    #[test]
    fn recent_imports_records_the_last_calls() {
        let page = TracePage::new(&[
            ("t!first".into(), sum5 as *const () as usize as u64),
            ("t!second".into(), scale as *const () as usize as u64),
        ])
        .unwrap();
        let f: extern "win64" fn(u64, u64, u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(page.addr(0) as usize) };
        let g: extern "win64" fn(f64, f64, f64, f64) -> f64 =
            unsafe { std::mem::transmute(page.addr(1) as usize) };
        f(0, 0, 0, 0, 0);
        g(0.0, 0.0, 0.0, 0.0);
        // Other tests of this module call traced imports in parallel: both names must be in the
        // ring, not necessarily adjacent to each other.
        let recent = recent_imports();
        assert!(recent.iter().any(|n| n == "t!first"));
        assert!(recent.iter().any(|n| n == "t!second"));
        // The ring is bounded.
        assert!(recent.len() <= RECENT_MAX);
    }
}
