//! Crash and timeout reporting for guest code.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

static IMAGE_BASE: AtomicU64 = AtomicU64::new(0);
static IMAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
const MAX_DLLS: usize = 32;
const NAME_LEN: usize = 32;
static DLL_COUNT: AtomicUsize = AtomicUsize::new(0);
static DLL_BASE: [AtomicU64; MAX_DLLS] = [const { AtomicU64::new(0) }; MAX_DLLS];
static DLL_END: [AtomicU64; MAX_DLLS] = [const { AtomicU64::new(0) }; MAX_DLLS];
static DLL_NAME: [[AtomicU8; NAME_LEN]; MAX_DLLS] =
    [const { [const { AtomicU8::new(0) }; NAME_LEN] }; MAX_DLLS];
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
    if sig == libc::SIGILL && unsafe { emulate_cpu_gap(uc as *mut libc::ucontext_t) } {
        return;
    }
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
        libc::SIGTRAP => "SIGTRAP",
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
    for i in 0..DLL_COUNT.load(Ordering::Acquire).min(MAX_DLLS) {
        let b = DLL_BASE[i].load(Ordering::Relaxed);
        if rip >= b && rip < DLL_END[i].load(Ordering::Relaxed) {
            let mut n = [0u8; NAME_LEN];
            for (d, s) in n.iter_mut().zip(&DLL_NAME[i]) {
                *d = s.load(Ordering::Relaxed);
            }
            let len = n.iter().position(|&c| c == 0).unwrap_or(NAME_LEN);
            say(format_args!(
                " ({}+{:#x})",
                String::from_utf8_lossy(&n[..len]),
                rip - b
            ));
        }
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

/// Installs the SIGSEGV/SIGILL/SIGBUS/SIGFPE/SIGTRAP reporter for a process whose guest image is at
/// `[base, base+size)`. Each guest thread must also call [`setup_thread`].
pub fn install(base: u64, size: usize) {
    IMAGE_BASE.store(base, Ordering::Relaxed);
    IMAGE_SIZE.store(size, Ordering::Relaxed);
    for sig in [
        libc::SIGSEGV,
        libc::SIGILL,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGTRAP,
    ] {
        unsafe {
            handle(
                sig,
                on_fault as extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) as usize,
                libc::SA_SIGINFO | libc::SA_ONSTACK,
            )
        };
    }
}

/// Registers a package DLL so that a fault inside it prints `name+0xOFF` (at most 32 DLLs; names
/// are cut to 31 bytes). Call before the guest runs.
pub fn register_dll(name: &str, base: u64, size: usize) {
    let i = DLL_COUNT.load(Ordering::Relaxed);
    if i >= MAX_DLLS {
        return;
    }
    for (k, c) in name.bytes().take(NAME_LEN - 1).enumerate() {
        DLL_NAME[i][k].store(c, Ordering::Relaxed);
    }
    DLL_BASE[i].store(base, Ordering::Relaxed);
    DLL_END[i].store(base + size as u64, Ordering::Relaxed);
    DLL_COUNT.store(i + 1, Ordering::Release);
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

/// Registers and flags of a faulted thread, in x86 encoding order (`rax`, `rcx`, `rdx`, `rbx`, `rsp`,
/// `rbp`, `rsi`, `rdi`, `r8`..`r15`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Regs {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub eflags: u64,
}

/// Index of the ucontext greg holding each x86-encoded register.
const GREG: [i32; 16] = [
    libc::REG_RAX,
    libc::REG_RCX,
    libc::REG_RDX,
    libc::REG_RBX,
    libc::REG_RSP,
    libc::REG_RBP,
    libc::REG_RSI,
    libc::REG_RDI,
    libc::REG_R8,
    libc::REG_R9,
    libc::REG_R10,
    libc::REG_R11,
    libc::REG_R12,
    libc::REG_R13,
    libc::REG_R14,
    libc::REG_R15,
];

/// Flags a `cmp` writes: CF, PF, AF, ZF, SF, OF.
const CMP_FLAGS: u64 = 0x8d5;

fn cmp_flags(a: u64, b: u64) -> u64 {
    let r = a.wrapping_sub(b);
    let mut f = 0;
    if a < b {
        f |= 1;
    }
    if (r as u8).count_ones().is_multiple_of(2) {
        f |= 1 << 2;
    }
    if (a ^ b ^ r) & 0x10 != 0 {
        f |= 1 << 4;
    }
    if r == 0 {
        f |= 1 << 6;
    }
    if r >> 63 != 0 {
        f |= 1 << 7;
    }
    if ((a ^ b) & (a ^ r)) >> 63 != 0 {
        f |= 1 << 11;
    }
    f
}

/// Decodes `REX.W cmp r/m64, r64` (`39 /r`, register form) into `(rm, reg)`: the instruction
/// computes `rm - reg`.
fn decode_cmp(b: &[u8]) -> Option<(usize, usize)> {
    let [rex @ (0x48 | 0x49 | 0x4c | 0x4d), 0x39, modrm] = *b else {
        return None;
    };
    if modrm >> 6 != 3 {
        return None;
    }
    let reg = ((modrm >> 3) & 7) as usize | ((rex as usize & 4) << 1);
    let rm = (modrm & 7) as usize | ((rex as usize & 1) << 3);
    Some((rm, reg))
}

/// Shape A tail after `clzero`: `add rax,0x40; cmp rax,b; jb -12`. Returns `b`.
fn shape_a(after: &[u8]) -> Option<usize> {
    let [0x48, 0x83, 0xc0, 0x40, cmp @ .., 0x72, 0xf4] = after else {
        return None;
    };
    match decode_cmp(cmp)? {
        (0, b) if b != 0 => Some(b),
        _ => None,
    }
}

/// Shape B around `clzero`: returns `(a, b)`.
fn shape_b(before: &[u8], after: &[u8]) -> Option<(usize, usize)> {
    let [rex @ (0x48 | 0x4c), 0x89, load, add_rex @ (0x48 | 0x49), 0x83, add, 0x40] = *before
    else {
        return None;
    };
    let [cmp @ .., 0x72, 0xf1] = after else {
        return None;
    };
    let (ca, cb) = decode_cmp(cmp)?;
    if load & 0xc7 != 0xc0 || add & 0xf8 != 0xc0 {
        return None;
    }
    let a = ((load >> 3) & 7) as usize | ((rex as usize & 4) << 1);
    let add_reg = (add & 7) as usize | ((add_rex as usize & 1) << 3);
    (a != 0 && a == add_reg && a == ca && cb != 0 && cb != a).then_some((a, cb))
}

/// Emulates the AMD-only `clzero` (`0f 01 fc`) the console CPU has and this host may lack.
///
/// `code` points at the faulting instruction; `back` and `fwd` are the numbers of bytes readable
/// before it and from it on. Zeroes the 64-byte line holding `rax`. The two loops compilers build
/// around it (`a`, `b` any registers but `rax`) are run whole, leaving registers and flags as their
/// last pass would, since each exception costs about a microsecond:
///
/// ```text
/// top: clzero              top: mov rax, a
///      add rax, 0x40            add a, 0x40
///      cmp rax, b               clzero
///      jb top                   cmp a, b
///                               jb top
/// ```
///
/// Anything else is one line and the next instruction. Returns `false` when `code` is not `clzero`.
///
/// # Safety
/// `code` must be readable as described, and the lines `regs` names are the guest's to zero.
pub(crate) unsafe fn emulate_clzero(
    regs: &mut Regs,
    code: *const u8,
    back: usize,
    fwd: usize,
) -> bool {
    if fwd < 3 || unsafe { std::slice::from_raw_parts(code, 3) } != [0x0f, 0x01, 0xfc] {
        return false;
    }
    let zero = |a: u64| unsafe { ((a & !63) as *mut u8).write_bytes(0, 64) };
    // Shape A: clzero; add rax,0x40; cmp rax,b; jb top.
    if fwd >= 12 {
        let after = unsafe { std::slice::from_raw_parts(code.add(3), 9) };
        if let Some(b) = shape_a(after) {
            let limit = regs.gpr[b];
            loop {
                zero(regs.gpr[0]);
                regs.gpr[0] = regs.gpr[0].wrapping_add(0x40);
                if regs.gpr[0] >= limit {
                    break;
                }
            }
            regs.eflags = regs.eflags & !CMP_FLAGS | cmp_flags(regs.gpr[0], limit);
            regs.rip += 12;
            return true;
        }
    }
    // Shape B: mov rax,a; add a,0x40; clzero; cmp a,b; jb top.
    if back >= 7 && fwd >= 8 {
        let (before, after) = unsafe {
            (
                std::slice::from_raw_parts(code.sub(7), 7),
                std::slice::from_raw_parts(code.add(3), 5),
            )
        };
        if let Some((a, cb)) = shape_b(before, after) {
            {
                let limit = regs.gpr[cb];
                loop {
                    zero(regs.gpr[0]);
                    if regs.gpr[a] >= limit {
                        break;
                    }
                    regs.gpr[0] = regs.gpr[a];
                    regs.gpr[a] = regs.gpr[a].wrapping_add(0x40);
                }
                regs.eflags = regs.eflags & !CMP_FLAGS | cmp_flags(regs.gpr[a], limit);
                regs.rip += 8;
                return true;
            }
        }
    }
    zero(regs.gpr[0]);
    regs.rip += 3;
    true
}

/// SIGILL for an instruction the console CPU has and the host lacks: emulates it and resumes.
/// Async-signal-safe: reads and writes only registers and guest memory.
unsafe fn emulate_cpu_gap(uc: *mut libc::ucontext_t) -> bool {
    let g = &mut (*uc).uc_mcontext.gregs;
    let rip = g[libc::REG_RIP as usize] as u64;
    if !in_guest_code(rip) {
        return false;
    }
    let mut regs = Regs {
        rip,
        eflags: g[libc::REG_EFL as usize] as u64,
        ..Regs::default()
    };
    for (r, i) in regs.gpr.iter_mut().zip(GREG) {
        *r = g[i as usize] as u64;
    }
    // Stay inside the page of the faulting instruction: the neighbouring one may be unmapped.
    let off = rip as usize & 0xfff;
    if !emulate_clzero(&mut regs, rip as *const u8, off, 0x1000 - off) {
        return false;
    }
    for (r, i) in regs.gpr.iter().zip(GREG) {
        g[i as usize] = *r as i64;
    }
    g[libc::REG_RIP as usize] = regs.rip as i64;
    g[libc::REG_EFL as usize] = regs.eflags as i64;
    true
}

fn in_guest_code(rip: u64) -> bool {
    let base = IMAGE_BASE.load(Ordering::Relaxed);
    if base != 0 && rip >= base && rip < base + IMAGE_SIZE.load(Ordering::Relaxed) as u64 {
        return true;
    }
    (0..DLL_COUNT.load(Ordering::Acquire).min(MAX_DLLS)).any(|i| {
        rip >= DLL_BASE[i].load(Ordering::Relaxed) && rip < DLL_END[i].load(Ordering::Relaxed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the emulator on `code` placed after `back` padding bytes, with `rip` standing for
    /// `code[back..]`.
    fn run(bytes: &[u8], back: usize, regs: &mut Regs) -> bool {
        let p = bytes.as_ptr();
        regs.rip = 0x1000;
        unsafe { emulate_clzero(regs, p.add(back), back, bytes.len() - back) }
    }

    fn line_at(mem: &mut [u8; 1024], off: usize) -> u64 {
        mem.as_mut_ptr() as u64 + off as u64
    }

    fn setup() -> Box<[u8; 1024]> {
        let mut m = Box::new([0xffu8; 1024]);
        // Align the working area.
        let _ = &mut m;
        m
    }

    fn base(m: &[u8; 1024]) -> u64 {
        (m.as_ptr() as u64 + 63) & !63
    }

    fn zeroed(m: &[u8; 1024], from: u64, to: u64) -> bool {
        let o = m.as_ptr() as u64;
        (0..1024u64).all(|i| (m[i as usize] == 0) == (o + i >= from && o + i < to))
    }

    const A_LOOP: [u8; 12] = [
        0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c, 0x39, 0xd0, 0x72, 0xf4,
    ];

    #[test]
    fn shape_a_every_register() {
        for b in (1..16usize).filter(|&b| b != 4) {
            let mut code = A_LOOP;
            // cmp rax, b: 39 /r with rm = rax, reg = b.
            code[7] = 0x48 | if b >= 8 { 4 } else { 0 };
            code[9] = 0xc0 | ((b as u8 & 7) << 3);
            let mut m = setup();
            let s = base(&m);
            let mut regs = Regs::default();
            regs.gpr[0] = s;
            regs.gpr[b] = s + 192;
            assert!(run(&code, 0, &mut regs));
            assert_eq!(regs.gpr[0], s + 192, "b={b}");
            assert_eq!(regs.rip, 0x1000 + 12);
            assert_eq!(regs.eflags & CMP_FLAGS, cmp_flags(s + 192, s + 192));
            assert!(zeroed(&m, s, s + 192));
            let _ = line_at(&mut m, 0);
        }
    }

    #[test]
    fn shape_a_unaligned_and_odd_bound() {
        let m = setup();
        let s = base(&m);
        let mut regs = Regs::default();
        regs.gpr[0] = s + 5;
        regs.gpr[10] = s + 100;
        assert!(run(&A_LOOP, 0, &mut regs));
        // Passes at s+5 and s+69; the next rax, s+133, is past the bound.
        assert_eq!(regs.gpr[0], s + 133);
        assert!(zeroed(&m, s, s + 128));
        assert_eq!(regs.eflags & CMP_FLAGS, cmp_flags(s + 133, s + 100));
    }

    #[test]
    fn shape_b() {
        // mov rax,r10; add r10,0x40; clzero; cmp r10,r11; jb top
        let code = [
            0x4c, 0x89, 0xd0, 0x49, 0x83, 0xc2, 0x40, 0x0f, 0x01, 0xfc, 0x4d, 0x39, 0xda, 0x72,
            0xf1,
        ];
        let m = setup();
        let s = base(&m);
        let mut regs = Regs::default();
        regs.gpr[0] = s;
        regs.gpr[10] = s + 64;
        regs.gpr[11] = s + 256;
        assert!(run(&code, 7, &mut regs));
        assert_eq!(regs.gpr[10], s + 256);
        assert_eq!(regs.gpr[0], s + 192);
        assert_eq!(regs.rip, 0x1000 + 8);
        assert!(zeroed(&m, s, s + 256));
        assert_eq!(regs.eflags & CMP_FLAGS, cmp_flags(s + 256, s + 256));
    }

    #[test]
    fn single_and_foreign_bytes() {
        let m = setup();
        let s = base(&m);
        let mut regs = Regs::default();
        regs.gpr[0] = s + 70;
        assert!(run(&[0x0f, 0x01, 0xfc, 0x90, 0x90], 0, &mut regs));
        assert_eq!(regs.rip, 0x1000 + 3);
        assert!(zeroed(&m, s + 64, s + 128));
        let mut regs = Regs::default();
        assert!(!run(&[0x0f, 0x01, 0xfd, 0x90], 0, &mut regs));
        assert!(!run(&[0x0f, 0x01], 0, &mut regs));
    }

    #[test]
    fn cpu_with_clzero_runs_real_loop() {
        // cpuid 0x80000008 EBX bit 0: CLZERO. Present: the CPU never traps, nothing to test.
        let has = std::arch::x86_64::__cpuid(0x8000_0008).ebx & 1 != 0;
        if has {
            return;
        }
        unsafe {
            let page = libc::mmap(
                std::ptr::null_mut(),
                8192,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            ) as *mut u8;
            assert_ne!(page as isize, -1);
            // mov rax, rdi; mov r10, rsi; top: clzero; add rax,0x40; cmp rax,r10; jb top; ret
            let code: [u8; 22] = [
                0x48, 0x89, 0xf8, 0x49, 0x89, 0xf2, 0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c,
                0x39, 0xd0, 0x72, 0xf4, 0x48, 0x89, 0xc0, 0xc3,
            ];
            std::ptr::copy_nonoverlapping(code.as_ptr(), page, code.len());
            let data = page.add(4096);
            std::ptr::write_bytes(data, 0xff, 4096);
            install(page as u64, 4096);
            setup_thread();
            let f: extern "C" fn(*mut u8, *mut u8) = std::mem::transmute(page);
            f(data.add(64), data.add(64 + 256));
            for i in 0..4096 {
                let inside = (64..64 + 256).contains(&i);
                assert_eq!(*data.add(i) == 0, inside, "byte {i}");
            }
        }
    }
}
