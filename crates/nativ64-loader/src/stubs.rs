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
