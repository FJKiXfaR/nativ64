use super::*;

// Programs are assembled with `llvm-mc -mcpu=gfx1013 -mattr=+wavefrontsize64`; branch targets
// are written as the signed word offset from the next instruction.

fn code(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn regs() -> EntryRegs {
    EntryRegs::new().with_sgprs(8..12).with_vgprs(0..4)
}

fn ir(words: &[u32]) -> Func {
    build(&code(words), 0, false, &regs()).unwrap()
}

/// Compares the printed IR with `want`, line by line, ignoring indentation.
fn check(f: &Func, want: &str) {
    let norm = |s: &str| {
        s.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
    };
    let got = f.to_string();
    assert_eq!(norm(&got), norm(want), "got:\n{got}");
}

#[test]
fn straight_line_folds_scalar_constants() {
    // s_mov_b32 s0, 5 ; s_add_u32 s0, s0, 7 ; v_add_f32 v2, s0, v1 ; s_waitcnt ; s_endpgm
    let f = ir(&[0xbe800385, 0x80008700, 0x06040200, 0xbf8cc07f, 0xbf810000]);
    check(
        &f,
        "wave64
         %0:v1 = arg
         %1:v2 = arg
         %2:exec_lo = arg
         %3:exec_hi = arg
         b0: ; +0x0 preds []
           %4:v2 = v_add_f32 #0xc, %0 ; exec_lo=%2, exec_hi=%3, v2=%1
           s_endpgm",
    );
    assert_eq!(f.blocks.len(), 1);
    assert_eq!(f.vals[4].def, ValDef::Inst { block: 0, index: 0 });
    assert_eq!(f.idom, vec![None]);
}

#[test]
fn if_else_through_execz_gets_a_phi() {
    let f = ir(&[
        0x7c020300, // v_cmp_lt_f32 vcc, v0, v1
        0xbe82246a, // s_and_saveexec_b64 s[2:3], vcc
        0xbf880002, // s_cbranch_execz 2
        0x7e0402f2, // v_mov_b32 v2, 1.0
        0xbf820001, // s_branch 1
        0x7e040280, // v_mov_b32 v2, 0
        0xbefe0402, // s_mov_b64 exec, s[2:3]
        0x06060502, // v_add_f32 v3, v2, v2
        0xbf810000, // s_endpgm
    ]);
    check(
        &f,
        "wave64
         %0:v0 = arg
         %1:v1 = arg
         %2:v2 = arg
         %3:v3 = arg
         %4:exec_lo = arg
         %5:exec_hi = arg
         b0: ; +0x0 preds []
           %6:vcc_lo, %7:vcc_hi = v_cmp_lt_f32 %0, %1 ; exec_lo=%4, exec_hi=%5
           %8:s2, %9:s3, %10:exec_lo, %11:exec_hi, %12:scc = s_and_saveexec_b64 {%6, %7} ; exec_lo=%4, exec_hi=%5
           s_cbranch_execz exec_lo=%10, exec_hi=%11 -> b2 else b1
         b1: ; +0xc preds [b0]
           %13:v2 = v_mov_b32 #0x3f800000 ; exec_lo=%10, exec_hi=%11, v2=%2
           jump b3
         b2: ; +0x14 preds [b0]
           %14:v2 = v_mov_b32 #0x0 ; exec_lo=%10, exec_hi=%11, v2=%2
           jump b3
         b3: ; +0x18 preds [b1 b2]
           %15:v2 = phi b1: %13, b2: %14
           %16:exec_lo, %17:exec_hi = s_mov_b64 {%8, %9}
           %18:v3 = v_add_f32 %15, %15 ; exec_lo=%16, exec_hi=%17, v3=%3
           s_endpgm",
    );
    assert_eq!(f.idom, vec![None, Some(0), Some(0), Some(0)]);
    assert_eq!(f.rpo[0], 0);
}

#[test]
fn loop_back_edge_gets_a_phi_and_prunes_the_dead_one() {
    // s_mov s0,0 ; L: s_add_u32 s0,s0,1 ; s_cmp_lt_u32 s0,4 ; s_cbranch_scc1 L ; s_endpgm
    let f = ir(&[0xbe800380, 0x80008100, 0xbf0a8400, 0xbf85fffd, 0xbf810000]);
    check(
        &f,
        "wave64
         b0: ; +0x0 preds []
           jump b1
         b1: ; +0x4 preds [b0 b1]
           %0:s0 = phi b0: #0x0, b1: %1
           %1:s0, %2:scc = s_add_u32 %0, #0x1
           %3:scc = s_cmp_lt_u32 %1, #0x4
           s_cbranch_scc1 scc=%3 -> b1 else b2
         b2: ; +0x10 preds [b1]
           s_endpgm",
    );
    assert_eq!(f.blocks[1].preds, vec![0, 1]);
}

#[test]
fn getpc_offset_and_constant_data_fold() {
    // s_getpc_b64 s[0:1] ; s_add_u32 s0, s0, 0x1c ; s_addc_u32 s1, s1, 0 ;
    // s_load_dwordx4 s[4:7], s[0:1], 0x10 ; s_waitcnt ; v_mov_b32 v0, s5 ; s_endpgm
    // then 16 bytes of padding and the constants the load reads at byte 48.
    let mut w = vec![
        0xbe801f00, 0x80009c00, 0x82018001, 0xf4080100, 0xfa000010, 0xbf8cc07f, 0x7e000205,
        0xbf810000,
    ];
    w.resize(12, 0);
    w.extend([0x11111111, 0x3f800000, 0x33333333, 0x44444444]);
    let f = ir(&w);
    check(
        &f,
        "wave64
         %0:v0 = arg
         %1:exec_lo = arg
         %2:exec_hi = arg
         b0: ; +0x0 preds []
           %3:v0 = v_mov_b32 #0x3f800000 ; exec_lo=%1, exec_hi=%2, v0=%0
           s_endpgm",
    );
}

#[test]
fn getpc_pair_is_symbolic_until_used() {
    // s_getpc_b64 s[0:1] ; s_add_u32 s0, s0, 0x1c ; s_addc_u32 s1, s1, 0 ;
    // s_load_dwordx4 s[4:7], s[0:1], 0x10 with the data out of range: the load stays.
    let f = ir(&[
        0xbe801f00, 0x80009c00, 0x82018001, 0xf4080100, 0xfa000010, 0xbf810000,
    ]);
    let text = f.to_string();
    assert!(
        text.contains("s_load_dwordx4 {code+0x20, code.hi} offset:16"),
        "{text}"
    );
}

#[test]
fn sixty_four_bit_pairs_split_into_dwords() {
    // s_mov_b64 s[4:5], s[8:9] ; s_and_b64 s[6:7], s[4:5], s[10:11] ;
    // v_cmp_eq_u32 vcc, s6, v0 ; v_cndmask_b32 v1, 0, 1, vcc ; s_endpgm
    let f = ir(&[
        0xbe840408, 0x87860a04, 0x7d840006, 0xd5010001, 0x01a90280, 0xbf810000,
    ]);
    check(
        &f,
        "wave64
         %0:s8 = arg
         %1:s9 = arg
         %2:s10 = arg
         %3:s11 = arg
         %4:v0 = arg
         %5:v1 = arg
         %6:exec_lo = arg
         %7:exec_hi = arg
         b0: ; +0x0 preds []
           %8:s4, %9:s5 = s_mov_b64 {%0, %1}
           %10:s6, %11:s7, %12:scc = s_and_b64 {%8, %9}, {%2, %3}
           %13:vcc_lo, %14:vcc_hi = v_cmp_eq_u32 %10, %4 ; exec_lo=%6, exec_hi=%7
           %15:v1 = v_cndmask_b32_e64 #0x0, #0x1, {%13, %14} ; exec_lo=%6, exec_hi=%7, v1=%5
           s_endpgm",
    );
}

#[test]
fn wave32_masks_are_one_dword() {
    // v_cmp_eq_u32 vcc_lo, s6, v0 in wave32
    let w = [0x7d840006u32, 0xbf810000];
    let f = build(
        &code(&w),
        0,
        true,
        &EntryRegs::new().with_sgprs(6..7).with_vgprs(0..1),
    )
    .unwrap();
    let text = f.to_string();
    assert!(text.starts_with("wave32"), "{text}");
    assert!(
        text.contains("%3:vcc_lo = v_cmp_eq_u32 %0, %1 ; exec_lo=%2"),
        "{text}"
    );
    assert!(
        !text.contains("exec_hi") && !text.contains("vcc_hi"),
        "{text}"
    );
}

#[test]
fn undefined_read_is_an_error_unless_an_entry_register() {
    // v_mov_b32 v0, v9 ; s_endpgm: v9 is not an entry register
    let e = build(&code(&[0x7e000309, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::UndefinedRead(Reg::V(9)));
    assert_eq!(e.at, 0);
    assert!(e.to_string().contains("v9"), "{e}");
    // s_mov_b32 s0, s3 : s3 is below the entry SGPRs
    let e = build(&code(&[0xbe800303, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::UndefinedRead(Reg::S(3)));
    // v_mov_b32 v0, v3 is fine: v3 is an argument
    let f = ir(&[0x7e000303, 0xbf810000]);
    assert!(f.args.iter().any(|(r, _)| *r == Reg::V(3)));
}

#[test]
fn indirect_jump_and_unknown_encoding_are_typed_errors() {
    // s_setpc_b64 s[8:9]
    let e = build(&code(&[0xbe802008, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
    // a first dword of no encoding (top bits 111001)
    let e = build(&code(&[0xe400_0000, 0, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert!(matches!(e.kind, IrErrorKind::Decode(_)), "{e}");
    // image_load v[0:3], v0, s[0:7] dmask:0xf dim:SQ_RSRC_IMG_1D: decoded, not in the IR yet
    let e = build(
        &code(&[0xf000_0f00, 0x0000_0000, 0xbf810000]),
        0,
        false,
        &regs(),
    )
    .unwrap_err();
    assert!(matches!(e.kind, IrErrorKind::UnsupportedEncoding(_)), "{e}");
    // v_pk_add_f16 v1, v4, v5: decoded, not in the IR yet
    let e = build(
        &code(&[0xcc0f_0001, 261 << 9 | 260, 0xbf810000]),
        0,
        false,
        &regs(),
    )
    .unwrap_err();
    assert!(matches!(e.kind, IrErrorKind::UnsupportedEncoding(_)), "{e}");
}

/// `NATIV64_SHADER_CORPUS=<dir of .dxbc blobs>`: builds the IR of every program that decodes
/// entirely and reports the successes and the most frequent errors. Never fails on an IR
/// error (they are the report); skipped when the variable is unset.
#[test]
fn corpus_builds() {
    let Some(dir) = std::env::var_os("NATIV64_SHADER_CORPUS") else {
        eprintln!("NATIV64_SHADER_CORPUS unset: corpus check skipped");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dxbc"))
        .collect();
    files.sort();
    // strict: user SGPRs from s8 (the VS/PS layout); wide: s0 too (what compute programs read)
    for first in [8, 0] {
        let regs = EntryRegs::new().with_sgprs(first..40).with_vgprs(0..16);
        let (mut programs, mut decoded, mut built, mut instrs, mut phis) = (0, 0, 0, 0, 0);
        let mut errors: BTreeMap<String, usize> = BTreeMap::new();
        for f in &files {
            let blob = std::fs::read(f).unwrap();
            let Ok(p) = crate::shader::parse(&blob) else {
                continue;
            };
            for (_, code) in p.native.slot_codes() {
                programs += 1;
                let wave32 = false;
                if gfx10::follow(code, gfx10::ENTRY, wave32).is_err() {
                    continue;
                }
                decoded += 1;
                match build(code, gfx10::ENTRY, wave32, &regs) {
                    Ok(func) => {
                        built += 1;
                        instrs += func.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
                        phis += func.blocks.iter().map(|b| b.phis.len()).sum::<usize>();
                    }
                    Err(e) => {
                        let key = match &e.kind {
                            IrErrorKind::UndefinedRead(r) => format!("undefined read of {r}"),
                            IrErrorKind::Unsupported(s) => s.clone(),
                            k => format!("{k:?}"),
                        };
                        *errors.entry(key).or_default() += 1;
                    }
                }
            }
        }
        eprintln!(
            "corpus ir (entry SGPRs from s{first}): {built}/{decoded} fully decoded programs \
             built ({programs} programs, {instrs} instructions, {phis} phis)"
        );
        let mut v: Vec<_> = errors.into_iter().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        for (k, n) in v.iter().take(15) {
            eprintln!("{n:6}  {k}");
        }
    }
}
