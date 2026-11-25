//! Crash and timeout reporting for guest code.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static IMAGE_BASE: AtomicU64 = AtomicU64::new(0);
static IMAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
const ALT_STACK: usize = 64 * 1024;

struct Buf {
    b: [u8; 512],
    n: usize,
}

impl std::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn say(args: std::fmt::Arguments) {
    use std::fmt::Write;
    let mut b = Buf { b: [0; 512], n: 0 };
    let _ = b.write_fmt(args);
    unsafe { libc::write(2, b.b.as_ptr().cast(), b.n) };
}

extern "C" fn on_fault(sig: i32, info: *mut libc::siginfo_t, uc: *mut libc::c_void) {
    let (addr, rip) = unsafe {
        let uc = uc as *mut libc::ucontext_t;
        (
            (*info).si_addr() as usize,
            (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64,
        )
    };
    let name = match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGILL => "SIGILL",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        _ => "signal",
    };
    say(format_args!(
        "nativ64: fatal {name} ({sig}) at address {addr:#x}, rip {rip:#x}"
    ));
    let base = IMAGE_BASE.load(Ordering::Relaxed);
    let size = IMAGE_SIZE.load(Ordering::Relaxed) as u64;
    if base != 0 && rip >= base && rip < base + size {
        say(format_args!(" (image+{:#x})", rip - base));
    }
    say(format_args!("\n"));
    unsafe { libc::_exit(139) }
}

extern "C" fn on_alarm(_: i32) {
    say(format_args!("nativ64: timeout\n"));
    unsafe { libc::_exit(124) }
}

unsafe fn handle(sig: i32, f: usize, flags: i32) {
    let mut sa: libc::sigaction = std::mem::zeroed();
    sa.sa_sigaction = f;
    sa.sa_flags = flags;
    libc::sigemptyset(&mut sa.sa_mask);
    libc::sigaction(sig, &sa, std::ptr::null_mut());
}

/// Installs the SIGSEGV/SIGILL/SIGBUS/SIGFPE reporter for a process whose guest image is at
/// `[base, base+size)`. Each guest thread must also call [`setup_thread`].
pub fn install(base: u64, size: usize) {
    IMAGE_BASE.store(base, Ordering::Relaxed);
    IMAGE_SIZE.store(size, Ordering::Relaxed);
    for sig in [libc::SIGSEGV, libc::SIGILL, libc::SIGBUS, libc::SIGFPE] {
        unsafe {
            handle(
                sig,
                on_fault as extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) as usize,
                libc::SA_SIGINFO | libc::SA_ONSTACK,
            )
        };
    }
}

/// Gives the calling thread an alternate signal stack (leaked: it lives as long as the thread).
pub fn setup_thread() {
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ALT_STACK,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        return;
    }
    let ss = libc::stack_t {
        ss_sp: mem,
        ss_flags: 0,
        ss_size: ALT_STACK,
    };
    unsafe { libc::sigaltstack(&ss, std::ptr::null_mut()) };
}

/// Exits with status 124 after `secs` seconds (0 disables).
pub fn set_timeout(secs: u32) {
    if secs == 0 {
        return;
    }
    unsafe {
        handle(libc::SIGALRM, on_alarm as extern "C" fn(i32) as usize, 0);
        libc::alarm(secs);
    }
}
