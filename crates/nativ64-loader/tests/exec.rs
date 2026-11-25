//! Tests that run real x86-64 code from a tiny in-memory PE32+.

use nativ64_loader::{Image, LoadError, LoadOptions};
use std::sync::Mutex;

/// The PEB is process-wide: tests that run guest code take turns.
static RUN: Mutex<()> = Mutex::new(());

const IMAGE_BASE: u64 = 0x1_4100_0000;
const TEXT_RVA: usize = 0x1000;
const TEXT_RAW: usize = 0x200;
const TEXT_RAW_SIZE: usize = 0xA00;
const TEXT_VSIZE: usize = 0x1000;

// Offsets inside .text
const CALLBACK: usize = 0x40;
const TLS_DIR: usize = 0x800;
const TLS_DATA: usize = 0x840; // 8 raw bytes
const TLS_INDEX: usize = 0x850;
const CALLBACKS: usize = 0x860; // [cb, 0]
const MARKER: usize = 0x880;
const RELOC: usize = 0x900;

const TLS_VALUE: u32 = 0xCAFE_F00D;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// `code` goes at the entry (.text+0); `callback` at .text+CALLBACK.
fn build(code: &[u8], callback: Option<&[u8]>, tls: bool) -> Vec<u8> {
    let mut f = vec![0u8; TEXT_RAW + TEXT_RAW_SIZE];
    f[..2].copy_from_slice(b"MZ");
    put32(&mut f, 0x3c, 0x80);
    let nt = 0x80;
    f[nt..nt + 4].copy_from_slice(b"PE\0\0");
    let fh = nt + 4;
    put16(&mut f, fh, 0x8664);
    put16(&mut f, fh + 2, 1);
    put16(&mut f, fh + 16, 240);
    put16(&mut f, fh + 18, 0x22);
    let opt = fh + 20;
    put16(&mut f, opt, 0x20b);
    put32(&mut f, opt + 16, TEXT_RVA as u32);
    put64(&mut f, opt + 24, IMAGE_BASE);
    put32(&mut f, opt + 32, 0x1000);
    put32(&mut f, opt + 36, 0x200);
    put32(&mut f, opt + 56, 0x2000);
    put32(&mut f, opt + 60, 0x200);
    put32(&mut f, opt + 108, 16);
    let sh = opt + 240;
    f[sh..sh + 5].copy_from_slice(b".text");
    put32(&mut f, sh + 8, TEXT_VSIZE as u32);
    put32(&mut f, sh + 12, TEXT_RVA as u32);
    put32(&mut f, sh + 16, TEXT_RAW_SIZE as u32);
    put32(&mut f, sh + 20, TEXT_RAW as u32);
    put32(&mut f, sh + 36, 0xE000_0020);

    let va = |off: usize| IMAGE_BASE + (TEXT_RVA + off) as u64;
    let t = &mut f[TEXT_RAW..];
    t[..code.len()].copy_from_slice(code);
    if let Some(cb) = callback {
        t[CALLBACK..CALLBACK + cb.len()].copy_from_slice(cb);
    }
    // the relocation block is always present: the preferred base may be taken
    put32(t, RELOC, TEXT_RVA as u32);
    put32(t, RELOC + 4, 8 + 6 * 2);
    let o = nt + 4 + 20;
    put32(&mut f, o + 112 + 5 * 8, (TEXT_RVA + RELOC) as u32);
    put32(&mut f, o + 112 + 5 * 8 + 4, 8 + 6 * 2);
    let t = &mut f[TEXT_RAW..];
    if tls {
        // IMAGE_TLS_DIRECTORY64
        put64(t, TLS_DIR, va(TLS_DATA));
        put64(t, TLS_DIR + 8, va(TLS_DATA + 8));
        put64(t, TLS_DIR + 16, va(TLS_INDEX));
        put64(t, TLS_DIR + 24, va(CALLBACKS));
        put32(t, TLS_DIR + 32, 8);
        put32(t, TLS_DATA, TLS_VALUE);
        put64(t, CALLBACKS, va(CALLBACK));
        // DIR64 relocations for the five pointers
        for (i, off) in [TLS_DIR, TLS_DIR + 8, TLS_DIR + 16, TLS_DIR + 24, CALLBACKS]
            .iter()
            .enumerate()
        {
            put16(t, RELOC + 8 + i * 2, (10 << 12) | *off as u16);
        }
        put32(&mut f, o + 112 + 9 * 8, (TEXT_RVA + TLS_DIR) as u32);
        put32(&mut f, o + 112 + 9 * 8 + 4, 40);
    }
    f
}

fn load(f: &[u8]) -> Image {
    Image::load(f, &LoadOptions::default()).unwrap()
}

/// disp32 for a RIP-relative operand ending at `end` (an offset in .text) targeting `target`.
fn rip(target: usize, end: usize) -> [u8; 4] {
    ((target as i64 - end as i64) as i32).to_le_bytes()
}

#[test]
fn entry_returns_a_constant() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    let img = load(&build(&[0xB8, 42, 0, 0, 0, 0xC3], None, false));
    assert_eq!(img.run_main().unwrap(), 42);
}

#[test]
fn entry_sees_the_teb_and_peb() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    // mov rax, gs:[0x60] ; mov rax, [rax+0x10] ; ret
    let code = [
        0x65, 0x48, 0x8B, 0x04, 0x25, 0x60, 0, 0, 0, 0x48, 0x8B, 0x40, 0x10, 0xC3,
    ];
    let img = load(&build(&code, None, false));
    assert_eq!(img.run_main().unwrap(), img.base() as u32);

    // TEB.Self.Self - TEB.Self == 0, and ExceptionList == -1 (low dword 0xFFFFFFFF)
    let code = [
        0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0, 0, 0, // mov rax, gs:[0x30]
        0x48, 0x8B, 0x40, 0x30, // mov rax, [rax+0x30]
        0x65, 0x48, 0x8B, 0x0C, 0x25, 0x30, 0, 0, 0, // mov rcx, gs:[0x30]
        0x48, 0x29, 0xC8, // sub rax, rcx
        0x65, 0x48, 0x8B, 0x0C, 0x25, 0, 0, 0, 0, // mov rcx, gs:[0]
        0x48, 0x01, 0xC8, // add rax, rcx
        0xC3,
    ];
    let img = load(&build(&code, None, false));
    assert_eq!(img.run_main().unwrap(), u32::MAX);
}

#[test]
fn tls_block_callback_and_slot() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    // callback(rcx = base, edx = reason): mov [rip+MARKER], edx ; ret
    let mut cb = vec![0x89, 0x15];
    cb.extend(rip(MARKER, CALLBACK + 6));
    cb.push(0xC3);
    // entry: mov rax, gs:[0x58] ; mov rax, [rax] ; mov eax, [rax] ; ret
    let code = [
        0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0x48, 0x8B, 0x00, 0x8B, 0x00, 0xC3,
    ];
    let img = load(&build(&code, Some(&cb), true));
    // the loader wrote TLS index 0
    assert_eq!(
        img.bytes_at((TEXT_RVA + TLS_INDEX) as u32, 4).unwrap(),
        [0; 4]
    );
    assert_eq!(img.run_main().unwrap(), TLS_VALUE);
    // DLL_PROCESS_ATTACH reached the callback before the entry point
    let m = img.bytes_at((TEXT_RVA + MARKER) as u32, 4).unwrap();
    assert_eq!(u32::from_le_bytes(m.try_into().unwrap()), 1);
}

#[test]
fn tls_survives_relocation() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    let code = [
        0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0x48, 0x8B, 0x00, 0x8B, 0x00, 0xC3,
    ];
    let f = build(&code, Some(&[0xC3]), true);
    let img = Image::load(
        &f,
        &LoadOptions {
            base_hint: Some(0x5100_0000_0000),
            ..LoadOptions::default()
        },
    )
    .unwrap();
    assert_ne!(img.base(), IMAGE_BASE);
    assert_eq!(img.run_main().unwrap(), TLS_VALUE);
}

#[test]
fn no_entry_point_is_an_error() {
    let mut f = build(&[0xC3], None, false);
    put32(&mut f, 0x80 + 4 + 20 + 16, 0);
    let img = load(&f);
    assert_eq!(img.run_main().unwrap_err(), LoadError::NoEntryPoint);
}

#[test]
fn tls_directory_out_of_bounds_is_an_error() {
    let mut f = build(&[0xC3], None, true);
    let o = 0x80 + 4 + 20;
    put32(&mut f, o + 112 + 9 * 8, 0x1_0000);
    assert!(matches!(
        Image::load(&f, &LoadOptions::default()),
        Err(LoadError::BadTls(_))
    ));
    // raw data range outside the image
    let mut f = build(&[0xC3], None, true);
    put64(&mut f, TEXT_RAW + TLS_DIR + 8, 0x7000_0000_0000);
    let r = Image::load(
        &f,
        &LoadOptions {
            base_hint: Some(IMAGE_BASE),
            ..LoadOptions::default()
        },
    );
    assert!(matches!(
        r,
        Err(LoadError::BadTls(_)) | Err(LoadError::RvaOutOfBounds(_))
    ));
}

const IMPORT_DIR: usize = 0x600;
const IMPORT_IAT: usize = 0x660;

/// Adds an import of `kernel32!GetCurrentProcessId` (IAT slot at .text+IMPORT_IAT).
fn with_import(mut f: Vec<u8>) -> Vec<u8> {
    let r = |off: usize| (TEXT_RVA + off) as u32;
    let o = 0x80 + 4 + 20;
    let t = &mut f[TEXT_RAW..];
    put32(t, IMPORT_DIR, r(0x640)); // OriginalFirstThunk
    put32(t, IMPORT_DIR + 12, r(0x6a0)); // Name
    put32(t, IMPORT_DIR + 16, r(IMPORT_IAT)); // FirstThunk
    put64(t, 0x640, r(0x680) as u64);
    put64(t, IMPORT_IAT, r(0x680) as u64);
    t[0x682..0x682 + 20].copy_from_slice(b"GetCurrentProcessId\0");
    t[0x6a0..0x6a0 + 13].copy_from_slice(b"KERNEL32.dll\0");
    put32(&mut f, o + 112 + 8, r(IMPORT_DIR));
    put32(&mut f, o + 112 + 8 + 4, 40);
    f
}

fn call_import_code() -> Vec<u8> {
    // call [rip+IAT] ; ret   (sub rsp,40 / add rsp,40 around the call for alignment)
    let mut c = vec![0x48, 0x83, 0xEC, 0x28, 0xFF, 0x15];
    c.extend(rip(IMPORT_IAT, 10));
    c.extend([0x48, 0x83, 0xC4, 0x28, 0xC3]);
    c
}

#[test]
fn imports_are_called_directly_by_default() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    let img = load(&with_import(build(&call_import_code(), None, false)));
    assert_eq!(img.run_main().unwrap(), std::process::id());
}

#[test]
fn traced_imports_go_through_a_trampoline() {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    let f = with_import(build(&call_import_code(), None, false));
    let img = Image::load(
        &f,
        &LoadOptions {
            trace_imports: true,
            ..LoadOptions::default()
        },
    )
    .unwrap();
    let before = nativ64_loader::trace_calls();
    assert_eq!(img.run_main().unwrap(), std::process::id());
    assert_eq!(nativ64_loader::trace_calls(), before + 1);
}
