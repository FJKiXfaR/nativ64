//! Loader tests on a tiny PE32+ built in memory.

use nativ64_loader::{Image, LoadError, LoadOptions, Pe};

const IMAGE_BASE: u64 = 0x1_4000_0000;
const TEXT_RVA: usize = 0x1000;
const TEXT_RAW: usize = 0x200;
const TEXT_RAW_SIZE: usize = 0x400;
const TEXT_VSIZE: usize = 0x800;

// Offsets inside .text
const PTR_SLOT: usize = 0x10;
const IMPORT_DIR: usize = 0x100;
const ILT: usize = 0x140;
const IAT: usize = 0x160;
const NAME1: usize = 0x180;
const NAME2: usize = 0x1a0;
const DLL: usize = 0x1c0;
const RELOC: usize = 0x200;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn build_pe() -> Vec<u8> {
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
    put32(&mut f, opt + 112 + 8, (TEXT_RVA + IMPORT_DIR) as u32);
    put32(&mut f, opt + 112 + 8 + 4, 40);
    put32(&mut f, opt + 112 + 5 * 8, (TEXT_RVA + RELOC) as u32);
    put32(&mut f, opt + 112 + 5 * 8 + 4, 12);
    let sh = opt + 240;
    f[sh..sh + 5].copy_from_slice(b".text");
    put32(&mut f, sh + 8, TEXT_VSIZE as u32);
    put32(&mut f, sh + 12, TEXT_RVA as u32);
    put32(&mut f, sh + 16, TEXT_RAW_SIZE as u32);
    put32(&mut f, sh + 20, TEXT_RAW as u32);
    put32(&mut f, sh + 36, 0xE000_0020);

    let t = &mut f[TEXT_RAW..];
    t[0] = 0xC3;
    put64(t, PTR_SLOT, IMAGE_BASE + TEXT_RVA as u64);
    // import descriptor: OFT, _, _, Name, FT; the next 20 bytes stay zero
    put32(t, IMPORT_DIR, (TEXT_RVA + ILT) as u32);
    put32(t, IMPORT_DIR + 12, (TEXT_RVA + DLL) as u32);
    put32(t, IMPORT_DIR + 16, (TEXT_RVA + IAT) as u32);
    put64(t, ILT, (TEXT_RVA + NAME1) as u64);
    put64(t, ILT + 8, (TEXT_RVA + NAME2) as u64);
    put64(t, IAT, (TEXT_RVA + NAME1) as u64);
    put64(t, IAT + 8, (TEXT_RVA + NAME2) as u64);
    t[NAME1 + 2..NAME1 + 2 + 18].copy_from_slice(b"GetCurrentThreadId");
    t[NAME2 + 2..NAME2 + 2 + 18].copy_from_slice(b"NoSuchFunction_xyz");
    t[DLL..DLL + 12].copy_from_slice(b"KERNEL32.dll");
    // one block for page 0x1000: a DIR64 entry for PTR_SLOT, then padding
    put32(t, RELOC, TEXT_RVA as u32);
    put32(t, RELOC + 4, 12);
    put16(t, RELOC + 8, (10 << 12) | PTR_SLOT as u16);
    put16(t, RELOC + 10, 0);
    f
}

fn rd64(img: &Image, rva: usize) -> u64 {
    u64::from_le_bytes(img.bytes_at(rva as u32, 8).unwrap().try_into().unwrap())
}

#[test]
fn parses() {
    let f = build_pe();
    let pe = Pe::parse(&f).unwrap();
    assert_eq!(pe.image_base, IMAGE_BASE);
    assert_eq!(pe.sections.len(), 1);
    assert_eq!(pe.sections[0].name, ".text");
    assert_eq!(pe.entry_rva, TEXT_RVA as u32);
    assert_eq!(pe.dirs[1].rva as usize, TEXT_RVA + IMPORT_DIR);
}

#[test]
fn maps_relocates_and_binds() {
    let f = build_pe();
    let want = 0x5000_0000_0000u64;
    let img = Image::load(
        &f,
        &LoadOptions {
            base_hint: Some(want),
        },
    )
    .unwrap();
    assert_ne!(img.base(), IMAGE_BASE);
    // relocated pointer follows the actual base
    assert_eq!(
        rd64(&img, TEXT_RVA + PTR_SLOT),
        img.base() + TEXT_RVA as u64
    );
    // code copied, bytes past the raw size are zero-filled
    assert_eq!(img.bytes_at(TEXT_RVA as u32, 1).unwrap()[0], 0xC3);
    assert_eq!(
        img.bytes_at((TEXT_RVA + TEXT_RAW_SIZE) as u32, 16).unwrap(),
        &[0u8; 16]
    );
    // known import points at nativ64's function
    let e = nativ64::lookup("kernel32", "GetCurrentThreadId").unwrap();
    assert_eq!(rd64(&img, TEXT_RVA + IAT), e.addr as u64);
    // unknown import goes to a stub, and is listed
    let stub = rd64(&img, TEXT_RVA + IAT + 8);
    assert_ne!(stub, 0);
    assert_ne!(stub, e.addr as u64);
    assert_eq!(img.imports.len(), 2);
    let un: Vec<_> = img.unresolved().map(|i| i.display()).collect();
    assert_eq!(un, ["KERNEL32.dll!NoSuchFunction_xyz"]);
    // stub starts with `mov ecx, imm32`
    let first = unsafe { *(stub as *const u8) };
    assert_eq!(first, 0xB9);
    assert_eq!(img.entry_point(), img.base() + TEXT_RVA as u64);
}

#[test]
fn preferred_base_needs_no_relocation() {
    let mut f = build_pe();
    // drop the relocation directory: loading at the preferred base must still work
    let opt = 0x80 + 4 + 20;
    put32(&mut f, opt + 112 + 5 * 8, 0);
    put32(&mut f, opt + 112 + 5 * 8 + 4, 0);
    match Image::load(&f, &LoadOptions::default()) {
        Ok(img) => assert_eq!(img.base(), IMAGE_BASE),
        // the preferred address was taken: then relocations are required
        Err(e) => assert_eq!(e, LoadError::NoRelocations),
    }
}

#[test]
fn rejects_truncated() {
    let f = build_pe();
    assert!(matches!(
        Pe::parse(&f[..0x90]),
        Err(LoadError::Truncated(_))
    ));
    assert!(matches!(Pe::parse(&f[..1]), Err(LoadError::Truncated(_))));
    // section table / raw data cut
    assert!(matches!(
        Pe::parse(&f[..TEXT_RAW + 0x10]),
        Err(LoadError::SectionOutOfBounds(_))
    ));
}

#[test]
fn rejects_bad_magic() {
    let mut f = build_pe();
    f[0] = b'X';
    assert_eq!(Pe::parse(&f).unwrap_err(), LoadError::BadDosMagic);
    let mut f = build_pe();
    f[0x80] = b'X';
    assert_eq!(Pe::parse(&f).unwrap_err(), LoadError::BadNtSignature);
}

#[test]
fn rejects_pe32() {
    let mut f = build_pe();
    put16(&mut f, 0x80 + 4 + 20, 0x10b);
    assert_eq!(Pe::parse(&f).unwrap_err(), LoadError::NotPe32Plus(0x10b));
}

#[test]
fn rejects_section_past_image() {
    let mut f = build_pe();
    let sh = 0x80 + 4 + 20 + 240;
    put32(&mut f, sh + 8, 0x10_0000);
    assert!(matches!(
        Pe::parse(&f),
        Err(LoadError::SectionOutOfBounds(_))
    ));
}

#[test]
fn rejects_unsupported_reloc() {
    let mut f = build_pe();
    put16(&mut f, TEXT_RAW + RELOC + 8, (3 << 12) | PTR_SLOT as u16);
    let r = Image::load(
        &f,
        &LoadOptions {
            base_hint: Some(0x5000_0000_0000),
        },
    );
    assert!(matches!(r, Err(LoadError::UnsupportedReloc(3))));
}
