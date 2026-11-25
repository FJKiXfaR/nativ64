//! Package DLLs: a tiny DLL with an export table and a DllMain, an EXE importing it.

use nativ64::process::{registered_module, ExportRef};
use nativ64_loader::{Image, LoadError, LoadOptions};
use std::path::PathBuf;
use std::sync::Mutex;

/// Guest code and the process-wide module registry: tests that run it take turns.
static RUN: Mutex<()> = Mutex::new(());

const EXE_BASE: u64 = 0x1_4200_0000;
const DLL_BASE: u64 = 0x1_4300_0000;
const TEXT_RVA: usize = 0x1000;
const TEXT_RAW: usize = 0x200;
const TEXT_RAW_SIZE: usize = 0xA00;

// Offsets inside the DLL's .text
const FUNC_ANSWER: usize = 0x100; // returns the DllMain call count
const FUNC_ORD: usize = 0x120; // returns 9, or the TLS value when the DLL has TLS
const TLS_DIR: usize = 0x300;
const TLS_DATA: usize = 0x340;
const TLS_INDEX: usize = 0x350;
const EXPORT_DIR: usize = 0x400;
const COUNTER: usize = 0x700;
const REASON: usize = 0x704;
const RELOC: usize = 0x900;

const TLS_VALUE: u32 = 0x0BAD_CAFE;
const ORDINAL_BASE: u32 = 5;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// disp32 for a RIP-relative operand ending at `end` targeting `target` (offsets in .text).
fn rip(target: usize, end: usize) -> [u8; 4] {
    ((target as i64 - end as i64) as i32).to_le_bytes()
}

/// A one-section PE32+ with a relocation block, ready for the caller to fill `.text`.
fn skeleton(base: u64, entry: usize) -> Vec<u8> {
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
    put32(&mut f, opt + 16, (TEXT_RVA + entry) as u32);
    put64(&mut f, opt + 24, base);
    put32(&mut f, opt + 32, 0x1000);
    put32(&mut f, opt + 36, 0x200);
    put32(&mut f, opt + 56, 0x2000);
    put32(&mut f, opt + 60, 0x200);
    put32(&mut f, opt + 108, 16);
    let sh = opt + 240;
    f[sh..sh + 5].copy_from_slice(b".text");
    put32(&mut f, sh + 8, 0x1000);
    put32(&mut f, sh + 12, TEXT_RVA as u32);
    put32(&mut f, sh + 16, TEXT_RAW_SIZE as u32);
    put32(&mut f, sh + 20, TEXT_RAW as u32);
    put32(&mut f, sh + 36, 0xE000_0020);
    put32(&mut f, opt + 112 + 5 * 8, (TEXT_RVA + RELOC) as u32);
    put32(&mut f, opt + 112 + 5 * 8 + 4, 16);
    put32(&mut f, TEXT_RAW + RELOC, TEXT_RVA as u32);
    put32(&mut f, TEXT_RAW + RELOC + 4, 16);
    f
}

fn dir_entry(f: &mut [u8], index: usize, rva: usize, size: usize) {
    let o = 0x80 + 4 + 20 + 112 + index * 8;
    put32(f, o, rva as u32);
    put32(f, o + 4, size as u32);
}

struct DllSpec {
    /// Name of the first export (the DllMain-count reader).
    first: &'static str,
    /// DllMain return value.
    main_returns: u32,
    tls: bool,
}

impl Default for DllSpec {
    fn default() -> Self {
        DllSpec {
            first: "answer",
            main_returns: 1,
            tls: false,
        }
    }
}

/// A DLL whose exports are `first` (ordinal 5), an unnamed one (6), `fwd` (7, forwarded to
/// `kernel32.GetCurrentProcessId`) and `badfwd` (8, forwarded to a module nobody has).
fn build_dll(spec: &DllSpec) -> Vec<u8> {
    let mut f = skeleton(DLL_BASE, 0);
    let va = |off: usize| DLL_BASE + (TEXT_RVA + off) as u64;
    let r = |off: usize| (TEXT_RVA + off) as u32;
    {
        let t = &mut f[TEXT_RAW..];
        // DllMain: inc [COUNTER] ; mov [REASON], edx ; mov eax, ret ; ret
        let mut c = vec![0xFF, 0x05];
        c.extend(rip(COUNTER, 6));
        c.extend([0x89, 0x15]);
        c.extend(rip(REASON, 12));
        c.push(0xB8);
        c.extend(spec.main_returns.to_le_bytes());
        c.push(0xC3);
        t[..c.len()].copy_from_slice(&c);
        // answer: mov eax, [COUNTER] ; ret
        let mut c = vec![0x8B, 0x05];
        c.extend(rip(COUNTER, FUNC_ANSWER + 6));
        c.push(0xC3);
        t[FUNC_ANSWER..FUNC_ANSWER + c.len()].copy_from_slice(&c);
        let ord_code: Vec<u8> = if spec.tls {
            // mov ecx,[TLS_INDEX] ; mov rax, gs:[0x58] ; mov rax,[rax+rcx*8] ; mov eax,[rax] ; ret
            let mut c = vec![0x8B, 0x0D];
            c.extend(rip(TLS_INDEX, FUNC_ORD + 6));
            c.extend([0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]);
            c.extend([0x48, 0x8B, 0x04, 0xC8, 0x8B, 0x00, 0xC3]);
            c
        } else {
            vec![0xB8, 9, 0, 0, 0, 0xC3]
        };
        t[FUNC_ORD..FUNC_ORD + ord_code.len()].copy_from_slice(&ord_code);

        // export directory: Base 5, 4 functions, 3 names
        let d = EXPORT_DIR;
        put32(t, d + 12, r(0x520));
        put32(t, d + 16, ORDINAL_BASE);
        put32(t, d + 20, 4);
        put32(t, d + 24, 3);
        put32(t, d + 28, r(0x440));
        put32(t, d + 32, r(0x460));
        put32(t, d + 36, r(0x480));
        put32(t, 0x440, r(FUNC_ANSWER));
        put32(t, 0x444, r(FUNC_ORD));
        put32(t, 0x448, r(0x4e0)); // forwarder strings lie inside the directory range
        put32(t, 0x44c, r(0x500));
        // names in lexical order: <first>, badfwd, fwd (the first one must sort before)
        put32(t, 0x460, r(0x4a0));
        put32(t, 0x464, r(0x4c0));
        put32(t, 0x468, r(0x4d0));
        put16(t, 0x480, 0);
        put16(t, 0x482, 3);
        put16(t, 0x484, 2);
        let s =
            |t: &mut [u8], at: usize, v: &str| t[at..at + v.len()].copy_from_slice(v.as_bytes());
        s(t, 0x4a0, spec.first);
        s(t, 0x4c0, "badfwd");
        s(t, 0x4d0, "fwd");
        s(t, 0x4e0, "kernel32.GetCurrentProcessId");
        s(t, 0x500, "nosuchmod.foo");
        s(t, 0x520, "testdll.dll");
        if spec.tls {
            put64(t, TLS_DIR, va(TLS_DATA));
            put64(t, TLS_DIR + 8, va(TLS_DATA + 4));
            put64(t, TLS_DIR + 16, va(TLS_INDEX));
            put32(t, TLS_DATA, TLS_VALUE);
            for (i, off) in [TLS_DIR, TLS_DIR + 8, TLS_DIR + 16].iter().enumerate() {
                put16(t, RELOC + 8 + i * 2, (10 << 12) | *off as u16);
            }
        }
    }
    dir_entry(&mut f, 0, TEXT_RVA + EXPORT_DIR, 0x200);
    if spec.tls {
        dir_entry(&mut f, 9, TEXT_RVA + TLS_DIR, 40);
    }
    f
}

enum Imp {
    Name(&'static str),
    Ordinal(u16),
}

const IAT: usize = 0x660;

/// An EXE importing one function of `module`; its entry calls it and returns EAX.
fn build_exe(module: &str, imp: Imp) -> Vec<u8> {
    let mut f = skeleton(EXE_BASE, 0);
    let r = |off: usize| (TEXT_RVA + off) as u32;
    {
        let t = &mut f[TEXT_RAW..];
        // sub rsp,40 ; call [rip+IAT] ; add rsp,40 ; ret
        let mut c = vec![0x48, 0x83, 0xEC, 0x28, 0xFF, 0x15];
        c.extend(rip(IAT, 10));
        c.extend([0x48, 0x83, 0xC4, 0x28, 0xC3]);
        t[..c.len()].copy_from_slice(&c);
        put32(t, 0x600, r(0x640)); // OriginalFirstThunk
        put32(t, 0x600 + 12, r(0x6a0)); // Name
        put32(t, 0x600 + 16, r(IAT)); // FirstThunk
        let thunk = match imp {
            Imp::Name(n) => {
                t[0x682..0x682 + n.len()].copy_from_slice(n.as_bytes());
                r(0x680) as u64
            }
            Imp::Ordinal(o) => 1 << 63 | o as u64,
        };
        put64(t, 0x640, thunk);
        put64(t, IAT, thunk);
        t[0x6a0..0x6a0 + module.len()].copy_from_slice(module.as_bytes());
    }
    dir_entry(&mut f, 1, TEXT_RVA + 0x600, 40);
    f
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nativ64-dll-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn opts(dir: &std::path::Path) -> LoadOptions {
    LoadOptions {
        search_dirs: vec![dir.to_path_buf()],
        ..LoadOptions::default()
    }
}

fn run(exe: &[u8], o: &LoadOptions) -> Result<u32, LoadError> {
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    Image::load(exe, o)?.run_main()
}

#[test]
fn exe_binds_to_the_dll_export_and_dllmain_runs_first() {
    let dir = temp_dir("bind");
    // The file name differs in case from the import.
    std::fs::write(dir.join("TestDLL.DLL"), build_dll(&DllSpec::default())).unwrap();
    let exe = build_exe("TESTDLL.dll", Imp::Name("answer"));
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    assert_eq!(img.dlls().len(), 1);
    assert_eq!(img.dlls()[0].name(), "testdll.dll");
    assert_eq!(img.unresolved().count(), 0);
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    // `answer` returns the number of DllMain calls: exactly one, before the entry point.
    assert_eq!(img.run_main().unwrap(), 1);
    let d = &img.dlls()[0];
    let word = |off: usize| {
        u32::from_le_bytes(
            d.bytes_at((TEXT_RVA + off) as u32, 4)
                .unwrap()
                .try_into()
                .unwrap(),
        )
    };
    assert_eq!(word(COUNTER), 1);
    assert_eq!(word(REASON), 1, "DLL_PROCESS_ATTACH");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ordinal_import_and_exports_by_ordinal() {
    let dir = temp_dir("ordinal");
    std::fs::write(dir.join("testdll.dll"), build_dll(&DllSpec::default())).unwrap();
    let exe = build_exe("testdll.dll", Imp::Ordinal(6));
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), 9);
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    let d = &img.dlls()[0];
    assert_eq!(
        d.export(ExportRef::Ordinal(5)),
        d.export(ExportRef::Name("answer"))
    );
    assert!(d.export(ExportRef::Ordinal(6)).is_some());
    assert!(d.export(ExportRef::Name("nothing")).is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn forwarders_resolve_to_nativ64_or_stay_unresolved() {
    let dir = temp_dir("fwd");
    std::fs::write(dir.join("testdll.dll"), build_dll(&DllSpec::default())).unwrap();
    let exe = build_exe("testdll.dll", Imp::Name("fwd"));
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), std::process::id());
    // The forwarder to a module nobody has gives a trap stub (reported as unresolved).
    let exe = build_exe("testdll.dll", Imp::Name("badfwd"));
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    assert_eq!(img.unresolved().count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn without_the_file_imports_come_from_nativ64() {
    let dir = temp_dir("absent");
    let exe = build_exe("KERNEL32.dll", Imp::Name("GetCurrentProcessId"));
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    assert!(img.dlls().is_empty());
    drop(img);
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), std::process::id());
    // A shipped DLL that lacks the function falls back to nativ64 too.
    std::fs::write(dir.join("kernel32.dll"), build_dll(&DllSpec::default())).unwrap();
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    assert_eq!(img.dlls().len(), 1);
    assert_eq!(img.unresolved().count(), 0);
    drop(img);
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), std::process::id());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn native_modules_override_the_shipped_dll() {
    let dir = temp_dir("native");
    let spec = DllSpec {
        first: "GetCurrentProcessId",
        ..DllSpec::default()
    };
    std::fs::write(dir.join("kernel32.dll"), build_dll(&spec)).unwrap();
    let exe = build_exe("KERNEL32.dll", Imp::Name("GetCurrentProcessId"));
    // The shipped DLL wins (its function returns the DllMain count).
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), 1);
    // Forced native: the DLL is not even mapped.
    let o = LoadOptions {
        native_modules: vec!["Kernel32".into()],
        ..opts(&dir)
    };
    assert!(Image::load(&exe, &o).unwrap().dlls().is_empty());
    assert_eq!(run(&exe, &o).unwrap(), std::process::id());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dllmain_returning_false_is_an_error() {
    let dir = temp_dir("false");
    let spec = DllSpec {
        main_returns: 0,
        ..DllSpec::default()
    };
    std::fs::write(dir.join("testdll.dll"), build_dll(&spec)).unwrap();
    let exe = build_exe("testdll.dll", Imp::Name("answer"));
    assert_eq!(
        run(&exe, &opts(&dir)).unwrap_err(),
        LoadError::DllInitFailed("testdll.dll".into())
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_export_directory_is_an_error() {
    let dir = temp_dir("badexp");
    let good = build_dll(&DllSpec::default());
    let exe = build_exe("testdll.dll", Imp::Name("answer"));
    let dir_at = TEXT_RAW + EXPORT_DIR;
    for (field, value) in [
        (28usize, 0x1_0000u32),
        (32, 0x1_0000),
        (36, 0x1_0000),
        (20, 0xFFFF_FFF0),
    ] {
        let mut bad = good.clone();
        put32(&mut bad, dir_at + field, value);
        std::fs::write(dir.join("testdll.dll"), &bad).unwrap();
        let e = Image::load(&exe, &opts(&dir)).err().unwrap();
        assert!(
            matches!(&e, LoadError::Dll(n, inner) if n == "testdll.dll"
                && matches!(**inner, LoadError::BadExports(_))),
            "field {field}: {e:?}"
        );
    }
    // The directory itself outside the image.
    let mut bad = good.clone();
    dir_entry(&mut bad, 0, 0x10_0000, 0x200);
    std::fs::write(dir.join("testdll.dll"), &bad).unwrap();
    assert!(matches!(
        Image::load(&exe, &opts(&dir)).err().unwrap(),
        LoadError::Dll(_, inner) if matches!(*inner, LoadError::BadExports(_))
    ));
    // A function RVA outside the image.
    let mut bad = good.clone();
    put32(&mut bad, TEXT_RAW + 0x440, 0x40_0000);
    std::fs::write(dir.join("testdll.dll"), &bad).unwrap();
    assert!(matches!(
        Image::load(&exe, &opts(&dir)).err().unwrap(),
        LoadError::Dll(_, inner) if matches!(*inner, LoadError::BadExports(_))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dll_static_tls_has_its_own_index() {
    let dir = temp_dir("tls");
    let spec = DllSpec {
        tls: true,
        ..DllSpec::default()
    };
    std::fs::write(dir.join("testdll.dll"), build_dll(&spec)).unwrap();
    let exe = build_exe("testdll.dll", Imp::Ordinal(6));
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    let d = &img.dlls()[0];
    assert_eq!(
        d.bytes_at((TEXT_RVA + TLS_INDEX) as u32, 4).unwrap(),
        1u32.to_le_bytes()
    );
    drop(img);
    assert_eq!(run(&exe, &opts(&dir)).unwrap(), TLS_VALUE);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn loaded_dlls_are_visible_through_the_module_apis() {
    use std::ffi::{c_char, c_void};
    let dir = temp_dir("registry");
    std::fs::write(dir.join("testdll.dll"), build_dll(&DllSpec::default())).unwrap();
    let exe = build_exe("testdll.dll", Imp::Name("answer"));
    let img = Image::load(&exe, &opts(&dir)).unwrap();
    let _g = RUN.lock().unwrap_or_else(|p| p.into_inner());
    img.run_main().unwrap();
    let base = img.dlls()[0].base() as usize;
    let m = registered_module("TestDll.dll").unwrap();
    assert_eq!(m.base, base);
    let get_module: extern "win64" fn(*const c_char) -> usize = unsafe {
        std::mem::transmute(
            nativ64::lookup("kernel32.dll", "GetModuleHandleA")
                .unwrap()
                .addr,
        )
    };
    let get_proc: extern "win64" fn(usize, *const c_char) -> *const c_void = unsafe {
        std::mem::transmute(
            nativ64::lookup("kernel32.dll", "GetProcAddress")
                .unwrap()
                .addr,
        )
    };
    assert_eq!(get_module(c"TESTDLL".as_ptr()), base);
    assert_eq!(get_module(c"testdll.dll".as_ptr()), base);
    let answer = img.dlls()[0].export(ExportRef::Name("answer")).unwrap();
    assert_eq!(get_proc(base, c"answer".as_ptr()) as u64, answer);
    assert_eq!(
        get_proc(base, 6 as *const c_char) as u64,
        img.dlls()[0].export(ExportRef::Ordinal(6)).unwrap()
    );
    assert!(get_proc(base, c"nothing".as_ptr()).is_null());
    nativ64::process::clear_modules();
    std::fs::remove_dir_all(&dir).unwrap();
}
