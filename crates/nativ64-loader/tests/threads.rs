//! Threads made by the guest through nativ64's `CreateThread`: TEB, static TLS and the
//! `DLL_THREAD_ATTACH/DETACH` notifications (TLS callbacks of the exe, `DllMain` of a DLL).

use nativ64_loader::{Image, LoadOptions};
use std::path::PathBuf;
use std::sync::Mutex;

/// Guest code and the process-wide registries: tests that run it take turns.
static RUN: Mutex<()> = Mutex::new(());

const EXE_BASE: u64 = 0x1_4400_0000;
const DLL_BASE: u64 = 0x1_4500_0000;
const DLL_BASE_NO_THREAD_CALLS: u64 = 0x1_4600_0000;
const TEXT_RVA: usize = 0x1000;
const TEXT_RAW: usize = 0x200;
const TEXT_RAW_SIZE: usize = 0xA00;
const STACK_RESERVE: u64 = 2 << 20;
const TLS_VALUE: u32 = 0xCAFE_F00D;

// Offsets inside .text
const THREAD_FN: usize = 0x100;
const CALLBACK: usize = 0x180;
const IMPORT_DIR: usize = 0x600;
const IAT: usize = 0x660;
const DLL_IAT: usize = 0x6f0;
const TLS_DIR: usize = 0x800;
const TLS_DATA: usize = 0x840;
const TLS_INDEX: usize = 0x850;
const CALLBACKS: usize = 0x860;
const RELOC: usize = 0x900;
// variables (exe), and the reason counters of the DLL
const COUNTS: usize = 0x740; // 4 x u32 indexed by reason
const SELF_T: usize = 0x760;
const SELFSELF_T: usize = 0x768;
const TID_T: usize = 0x770;
const BLK_T: usize = 0x778;
const BLK_MAIN: usize = 0x780;
const TID: usize = 0x788;
const VAL_T: usize = 0x78c;
const VAL_MAIN2: usize = 0x790;
const STK_T: usize = 0x798;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// A one-section PE32+ whose relocation block (DIR64 entries) is filled by `finish`.
struct Img {
    f: Vec<u8>,
    relocs: Vec<usize>,
}

impl Img {
    fn new(base: u64) -> Img {
        let mut f = vec![0u8; TEXT_RAW + TEXT_RAW_SIZE];
        f[..2].copy_from_slice(b"MZ");
        put32(&mut f, 0x3c, 0x80);
        f[0x80..0x84].copy_from_slice(b"PE\0\0");
        let fh = 0x84;
        put16(&mut f, fh, 0x8664);
        put16(&mut f, fh + 2, 1);
        put16(&mut f, fh + 16, 240);
        put16(&mut f, fh + 18, 0x22);
        let opt = fh + 20;
        put16(&mut f, opt, 0x20b);
        put32(&mut f, opt + 16, TEXT_RVA as u32);
        put64(&mut f, opt + 24, base);
        put32(&mut f, opt + 32, 0x1000);
        put32(&mut f, opt + 36, 0x200);
        put32(&mut f, opt + 56, 0x2000);
        put32(&mut f, opt + 60, 0x200);
        put64(&mut f, opt + 72, STACK_RESERVE);
        put32(&mut f, opt + 108, 16);
        let sh = opt + 240;
        f[sh..sh + 5].copy_from_slice(b".text");
        put32(&mut f, sh + 8, 0x1000);
        put32(&mut f, sh + 12, TEXT_RVA as u32);
        put32(&mut f, sh + 16, TEXT_RAW_SIZE as u32);
        put32(&mut f, sh + 20, TEXT_RAW as u32);
        put32(&mut f, sh + 36, 0xE000_0020);
        Img {
            f,
            relocs: Vec::new(),
        }
    }
    fn text(&mut self, off: usize, bytes: &[u8]) {
        self.f[TEXT_RAW + off..TEXT_RAW + off + bytes.len()].copy_from_slice(bytes);
    }
    fn dir(&mut self, index: usize, off: usize, size: usize) {
        let o = 0x84 + 20 + 112 + index * 8;
        put32(&mut self.f, o, (TEXT_RVA + off) as u32);
        put32(&mut self.f, o + 4, size as u32);
    }
    fn finish(mut self) -> Vec<u8> {
        let n = self.relocs.len();
        let size = 8 + 2 * (n + n % 2);
        let t = TEXT_RAW + RELOC;
        put32(&mut self.f, t, TEXT_RVA as u32);
        put32(&mut self.f, t + 4, size as u32);
        for (i, off) in self.relocs.clone().into_iter().enumerate() {
            put16(&mut self.f, t + 8 + i * 2, (10 << 12) | off as u16);
        }
        self.dir(5, RELOC, size);
        self.f
    }
}

/// Appends machine code at .text offset `at`, with RIP-relative operands resolved.
struct Code {
    at: usize,
    b: Vec<u8>,
}

impl Code {
    fn new(at: usize) -> Code {
        Code { at, b: Vec::new() }
    }
    fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.b.extend_from_slice(bytes);
        self
    }
    /// `prefix` then a disp32 to `target`; no immediate follows.
    fn rip(&mut self, prefix: &[u8], target: usize) -> &mut Self {
        self.b.extend_from_slice(prefix);
        let end = self.at + self.b.len() + 4;
        self.b
            .extend_from_slice(&((target as i64 - end as i64) as i32).to_le_bytes());
        self
    }
}

const GS_SELF: [u8; 9] = [0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0, 0, 0];
const GS_TID: [u8; 9] = [0x65, 0x48, 0x8B, 0x04, 0x25, 0x48, 0, 0, 0];
const GS_TLS: [u8; 9] = [0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0];
const GS_STACK_BASE: [u8; 9] = [0x65, 0x48, 0x8B, 0x04, 0x25, 0x08, 0, 0, 0];
const GS_STACK_LIMIT_SUB: [u8; 9] = [0x65, 0x48, 0x2B, 0x04, 0x25, 0x10, 0, 0, 0];

fn exe() -> Vec<u8> {
    let mut img = Img::new(EXE_BASE);
    let va = |off: usize| EXE_BASE + (TEXT_RVA + off) as u64;
    let r = |off: usize| (TEXT_RVA + off) as u32;

    // entry: remember this thread's TLS block, CreateThread(0, 0, THREAD_FN, 0, 0, &TID), wait,
    // then read this thread's TLS value again.
    let mut c = Code::new(0);
    c.raw(&[0x48, 0x83, 0xEC, 0x38]) // sub rsp, 56
        .raw(&GS_TLS)
        .raw(&[0x48, 0x8B, 0x00]) // mov rax, [rax]
        .rip(&[0x48, 0x89, 0x05], BLK_MAIN)
        .raw(&[0x31, 0xC9, 0x31, 0xD2]) // xor ecx,ecx ; xor edx,edx
        .rip(&[0x4C, 0x8D, 0x05], THREAD_FN) // lea r8, [THREAD_FN]
        .raw(&[0x45, 0x31, 0xC9]) // xor r9d, r9d
        .raw(&[0x48, 0xC7, 0x44, 0x24, 0x20, 0, 0, 0, 0]) // mov qword [rsp+32], 0
        .rip(&[0x48, 0x8D, 0x05], TID) // lea rax, [TID]
        .raw(&[0x48, 0x89, 0x44, 0x24, 0x28]) // mov [rsp+40], rax
        .rip(&[0xFF, 0x15], IAT) // call [CreateThread]
        .raw(&[0x48, 0x89, 0xC1, 0xBA, 0xFF, 0xFF, 0xFF, 0xFF]) // mov rcx,rax ; mov edx,-1
        .rip(&[0xFF, 0x15], IAT + 8) // call [WaitForSingleObject]
        .raw(&GS_TLS)
        .raw(&[0x48, 0x8B, 0x00, 0x8B, 0x00]) // mov rax,[rax] ; mov eax,[rax]
        .rip(&[0x89, 0x05], VAL_MAIN2)
        .raw(&[0x31, 0xC0, 0x48, 0x83, 0xC4, 0x38, 0xC3]); // xor eax,eax ; add rsp,56 ; ret
    img.text(0, &c.b);

    // the guest thread: record what it sees through gs
    let mut c = Code::new(THREAD_FN);
    c.raw(&GS_SELF)
        .rip(&[0x48, 0x89, 0x05], SELF_T)
        .raw(&[0x48, 0x8B, 0x40, 0x30]) // mov rax, [rax+0x30]
        .rip(&[0x48, 0x89, 0x05], SELFSELF_T)
        .raw(&GS_TID)
        .rip(&[0x48, 0x89, 0x05], TID_T)
        .raw(&GS_STACK_BASE)
        .raw(&GS_STACK_LIMIT_SUB)
        .rip(&[0x48, 0x89, 0x05], STK_T)
        .raw(&GS_TLS)
        .raw(&[0x48, 0x8B, 0x00]) // mov rax, [rax]
        .rip(&[0x48, 0x89, 0x05], BLK_T)
        .raw(&[0x8B, 0x08]) // mov ecx, [rax]
        .rip(&[0x89, 0x0D], VAL_T)
        .raw(&[0xC7, 0x00, 0x34, 0x12, 0, 0]) // mov dword [rax], 0x1234
        .raw(&[0xB8, 7, 0, 0, 0, 0xC3]); // mov eax, 7 ; ret
    img.text(THREAD_FN, &c.b);

    // TLS callback: mov edx,edx ; lea r8,[COUNTS] ; inc dword [r8+rdx*4] ; ret
    let mut c = Code::new(CALLBACK);
    c.raw(&[0x89, 0xD2])
        .rip(&[0x4C, 0x8D, 0x05], COUNTS)
        .raw(&[0x41, 0xFF, 0x04, 0x90, 0xC3]);
    img.text(CALLBACK, &c.b);

    // imports: kernel32!CreateThread, WaitForSingleObject; tdll.dll!answer (to load the DLL)
    let mut t = vec![0u8; 0x200];
    put32(&mut t, 0, r(0x640));
    put32(&mut t, 12, r(0x6a0));
    put32(&mut t, 16, r(IAT));
    put32(&mut t, 20, r(0x6e0));
    put32(&mut t, 20 + 12, r(0x720));
    put32(&mut t, 20 + 16, r(DLL_IAT));
    img.text(IMPORT_DIR, &t[..40]);
    for (at, name_rva) in [(0x640, 0x680), (IAT, 0x680)] {
        put64(&mut t, at - IMPORT_DIR, r(name_rva) as u64);
        put64(&mut t, at - IMPORT_DIR + 8, r(0x6c0) as u64);
    }
    for at in [0x6e0, DLL_IAT] {
        put64(&mut t, at - IMPORT_DIR, r(0x700) as u64);
    }
    img.text(0x640, &t[0x40..0x58]);
    img.text(IAT, &t[0x60..0x78]);
    img.text(0x6e0, &t[0xe0..0xe8]);
    img.text(DLL_IAT, &t[0xf0..0xf8]);
    img.text(0x682, b"CreateThread\0");
    img.text(0x6a0, b"KERNEL32.dll\0");
    img.text(0x6c2, b"WaitForSingleObject\0");
    img.text(0x702, b"answer\0");
    img.text(0x720, b"tdll.dll\0");
    img.dir(1, IMPORT_DIR, 60);

    // TLS directory: 4 raw bytes (TLS_VALUE), index slot, one callback
    let mut t = vec![0u8; 0x80];
    put64(&mut t, 0, va(TLS_DATA));
    put64(&mut t, 8, va(TLS_DATA + 4));
    put64(&mut t, 16, va(TLS_INDEX));
    put64(&mut t, 24, va(CALLBACKS));
    img.text(TLS_DIR, &t[..40]);
    img.text(TLS_DATA, &TLS_VALUE.to_le_bytes());
    img.text(CALLBACKS, &va(CALLBACK).to_le_bytes());
    img.relocs
        .extend([TLS_DIR, TLS_DIR + 8, TLS_DIR + 16, TLS_DIR + 24, CALLBACKS]);
    img.dir(9, TLS_DIR, 40);
    img.finish()
}

/// `tdll.dll`: export `answer`; `DllMain` counts its calls per reason in `COUNTS`.
fn dll(base: u64) -> Vec<u8> {
    let mut img = Img::new(base);
    let r = |off: usize| (TEXT_RVA + off) as u32;
    // DllMain: mov edx,edx ; lea r8,[COUNTS] ; inc dword [r8+rdx*4] ; mov eax,1 ; ret
    let mut c = Code::new(0);
    c.raw(&[0x89, 0xD2])
        .rip(&[0x4C, 0x8D, 0x05], COUNTS)
        .raw(&[0x41, 0xFF, 0x04, 0x90, 0xB8, 1, 0, 0, 0, 0xC3]);
    img.text(0, &c.b);
    img.text(0x100, &[0xB8, 1, 0, 0, 0, 0xC3]); // answer
    let mut t = vec![0u8; 0x200];
    put32(&mut t, 12, r(0x520));
    put32(&mut t, 16, 1);
    put32(&mut t, 20, 1);
    put32(&mut t, 24, 1);
    put32(&mut t, 28, r(0x40));
    put32(&mut t, 32, r(0x60));
    put32(&mut t, 36, r(0x80));
    put32(&mut t, 0x40, r(0x100));
    put32(&mut t, 0x60, r(0xa0));
    t[0xa0..0xa7].copy_from_slice(b"answer\0");
    t[0x120..0x129].copy_from_slice(b"tdll.dll\0");
    put32(&mut t, 12, r(0x400 + 0x120));
    put32(&mut t, 28, r(0x400 + 0x40));
    put32(&mut t, 32, r(0x400 + 0x60));
    put32(&mut t, 36, r(0x400 + 0x80));
    put32(&mut t, 0x60, r(0x400 + 0xa0));
    put32(&mut t, 0x40, r(0x100));
    img.text(0x400, &t);
    img.dir(0, 0x400, 0x200);
    img.finish()
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nativ64-thr-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn u32_at(img: &Image, off: usize) -> u32 {
    let b = img.bytes_at((TEXT_RVA + off) as u32, 4).unwrap();
    u32::from_le_bytes(b.try_into().unwrap())
}
fn u64_at(img: &Image, off: usize) -> u64 {
    let b = img.bytes_at((TEXT_RVA + off) as u32, 8).unwrap();
    u64::from_le_bytes(b.try_into().unwrap())
}

/// Loads the exe and its DLL (built for `dll_base`), runs it and hands the image to `check`.
fn run(dll_base: u64, before: impl FnOnce(&Image), check: impl FnOnce(&Image)) {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    let dir = temp_dir(&format!("{dll_base:x}"));
    std::fs::write(dir.join("tdll.dll"), dll(dll_base)).unwrap();
    let img = Image::load(
        &exe(),
        &LoadOptions {
            search_dirs: vec![dir],
            ..LoadOptions::default()
        },
    )
    .unwrap();
    assert_eq!(img.dlls().len(), 1);
    before(&img);
    assert_eq!(img.run_main().unwrap(), 0);
    check(&img);
}

#[test]
fn thread_has_its_own_teb_tls_and_stack() {
    run(
        DLL_BASE,
        |_| {},
        |img| {
            let teb = u64_at(img, SELF_T);
            assert_ne!(teb, 0);
            assert_eq!(u64_at(img, SELFSELF_T), teb, "gs:[0x30] is a self pointer");
            assert_eq!(
                u64_at(img, TID_T) as u32,
                u32_at(img, TID),
                "ClientId thread"
            );
            let (main, thread) = (u64_at(img, BLK_MAIN), u64_at(img, BLK_T));
            assert!(main != 0 && thread != 0 && main != thread, "own TLS block");
            assert_eq!(u32_at(img, VAL_T), TLS_VALUE, "TLS template copied");
            assert_eq!(
                u32_at(img, VAL_MAIN2),
                TLS_VALUE,
                "main block untouched by the thread"
            );
            let stack = u64_at(img, STK_T);
            assert!(
                (STACK_RESERVE - 0x1_0000..=STACK_RESERVE + 0x1_0000).contains(&stack),
                "stack size {stack:#x} follows the exe's SizeOfStackReserve"
            );
        },
    );
}

#[test]
fn thread_attach_and_detach_reach_callbacks_and_dllmain() {
    run(
        DLL_BASE,
        |_| {},
        |img| {
            // TLS callback of the exe: process attach once, then the thread's attach and detach
            for reason in [1, 2, 3] {
                assert_eq!(u32_at(img, COUNTS + reason * 4), 1, "exe reason {reason}");
            }
            // DllMain of the DLL: its process attach is not a thread notification, ours are
            let d = &img.dlls()[0];
            let n = |reason: usize| {
                let b = d
                    .bytes_at((TEXT_RVA + COUNTS + reason * 4) as u32, 4)
                    .unwrap();
                u32::from_le_bytes(b.try_into().unwrap())
            };
            assert_eq!((n(1), n(2), n(3)), (1, 1, 1));
        },
    );
}

#[test]
fn disable_thread_library_calls_silences_dllmain_only() {
    run(
        DLL_BASE_NO_THREAD_CALLS,
        |img| nativ64::process::disable_thread_calls(img.dlls()[0].base() as usize),
        |img| {
            let d = &img.dlls()[0];
            let n = |reason: usize| {
                let b = d
                    .bytes_at((TEXT_RVA + COUNTS + reason * 4) as u32, 4)
                    .unwrap();
                u32::from_le_bytes(b.try_into().unwrap())
            };
            assert_eq!((n(1), n(2), n(3)), (1, 0, 0));
            // the exe's TLS callbacks are not affected by it
            assert_eq!((u32_at(img, COUNTS + 8), u32_at(img, COUNTS + 12)), (1, 1));
        },
    );
}
