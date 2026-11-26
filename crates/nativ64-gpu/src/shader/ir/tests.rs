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
fn wrexec_writes_the_sgpr_then_exec() {
    let f = ir(&[
        0x7c020300, // v_cmp_lt_f32 vcc, v0, v1
        0xbe823a6a, // s_andn2_wrexec_b64 s[2:3], vcc
        0xbf810000, // s_endpgm
    ]);
    check(
        &f,
        "wave64
         %0:v0 = arg
         %1:v1 = arg
         %2:exec_lo = arg
         %3:exec_hi = arg
         b0: ; +0x0 preds []
           %4:vcc_lo, %5:vcc_hi = v_cmp_lt_f32 %0, %1 ; exec_lo=%2, exec_hi=%3
           %6:s2, %7:s3, %8:exec_lo, %9:exec_hi, %10:scc = s_andn2_wrexec_b64 {%4, %5} ; exec_lo=%2, exec_hi=%3
           s_endpgm",
    );
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
}

/// `s_getpc_b64 s[0:1]; s_add_u32 s0, s0, <literal>; s_addc_u32 s1, s1, 0; s_setpc_b64 s[0:1]`
/// at byte 0, 16 bytes of code in all, the jump landing `delta` bytes after the `s_getpc` end.
fn far_jump(delta: u32) -> Vec<u32> {
    vec![0xbe801f00, 0x8000ff00, delta, 0x82018001, 0xbe802000]
}

#[test]
fn far_jump_through_getpc_is_a_direct_jump() {
    // The code after the jump is skipped; the target at byte 28 sets v0 and ends. The jump's
    // base is the end of s_getpc (byte 4), so the literal is 28 - 4.
    let mut w = far_jump(24);
    w.extend([0xbf810000, 0xbf810000]); // bytes 20 and 24: dead code
    w.extend([0x7e0002f2, 0xbf810000]); // byte 28: v_mov_b32 v0, 1.0 ; s_endpgm
    let f = ir(&w);
    check(
        &f,
        "wave64
         %0:v0 = arg
         %1:exec_lo = arg
         %2:exec_hi = arg
         b0: ; +0x0 preds []
           jump b1
         b1: ; +0x1c preds [b0]
           %3:v0 = v_mov_b32 #0x3f800000 ; exec_lo=%1, exec_hi=%2, v0=%0
           s_endpgm",
    );
    assert_eq!(f.idom, vec![None, Some(0)]);
}

#[test]
fn far_jump_that_does_not_fold_or_leaves_the_code_is_an_error() {
    // The offset comes from an SGPR argument: s_add_u32 s0, s0, s8
    let e = build(
        &code(&[0xbe801f00, 0x80000800, 0x82018001, 0xbe802000, 0xbf810000]),
        0,
        false,
        &regs(),
    )
    .unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
    assert_eq!(e.at, 12);
    // The target is past the code, or not word aligned.
    for delta in [0x1000, 21] {
        let mut w = far_jump(delta);
        w.push(0xbf810000);
        let e = build(&code(&w), 0, false, &regs()).unwrap_err();
        assert!(
            matches!(&e.kind, IrErrorKind::Decode(d) if matches!(d.kind, gfx10::ErrorKind::BadTarget(_))),
            "{e}"
        );
    }
    // A jump to the far jump itself folds to a loop of one block: still a jump, not an error.
    let f = ir(&far_jump(0u32.wrapping_sub(4)));
    assert_eq!(f.blocks.len(), 1);
    assert_eq!(f.blocks[0].term, Term::Jump(0));
}

#[test]
fn far_jump_back_through_sub_and_subb_is_a_direct_jump() {
    // s_branch LJ ; LT: s_endpgm ; LJ: s_getpc_b64 s[8:9] ; s_sub_u32 s8, s8, 8 ;
    // s_subb_u32 s9, s9, 0 ; s_setpc_b64 s[8:9]: the jump lands on LT, 8 bytes before the
    // end of the `s_getpc`.
    let f = ir(&[
        0xbf820001, 0xbf810000, 0xbe881f00, 0x80888808, 0x82898009, 0xbe802008,
    ]);
    assert_eq!(f.blocks.len(), 3);
    assert_eq!(f.blocks[2].term, Term::Jump(1));
    assert_eq!(f.blocks[1].term, Term::End);
}

#[test]
fn two_far_jumps_in_a_row_are_followed() {
    // byte 0: far jump to byte 20; byte 20: far jump to byte 40 (s_getpc ends at 24);
    // byte 40: s_endpgm
    let mut w = far_jump(16);
    w.extend(far_jump(40 - 24));
    w.resize(10, 0xbf810000);
    w.push(0xbf810000);
    let f = ir(&w);
    assert_eq!(f.blocks.len(), 3);
    assert_eq!(f.blocks[2].term, Term::End);
}

fn fetch_regs() -> EntryRegs {
    EntryRegs::new().with_vgprs(0..4).with_fetch(vec![4, 5])
}

// s_swappc_b64 s[4:5], s[4:5] ; v_add_f32 v0, v4, v5 ; s_endpgm
const FETCH_PROGRAM: [u32; 3] = [0xbe842104, 0x06000b04, 0xbf810000];

#[test]
fn vertex_fetch_call_defines_the_attribute_vgprs() {
    let f = build(&code(&FETCH_PROGRAM), 0, false, &fetch_regs()).unwrap();
    check(
        &f,
        "wave64
         %0:s4 = arg
         %1:s5 = arg
         %2:v0 = arg
         %3:exec_lo = arg
         %4:exec_hi = arg
         b0: ; +0x0 preds []
           %5:v4, %6:v5, %7:s4, %8:s5 = s_swappc_b64 {%0, %1} ; exec_lo=%3, exec_hi=%4
           %9:v0 = v_add_f32 %5, %6 ; exec_lo=%3, exec_hi=%4, v0=%2
           s_endpgm",
    );
    // Without the attribute list the same call is a plain register jump.
    let e = build(&code(&FETCH_PROGRAM), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
    // A call that is not the program's first instruction stays one too.
    let mut w = vec![0xbe800380]; // s_mov_b32 s0, 0
    w.extend(FETCH_PROGRAM);
    let e = build(&code(&w), 0, false, &fetch_regs()).unwrap_err();
    assert_eq!((e.kind, e.at), (IrErrorKind::Indirect, 4));
    // So does a call through other registers: s_swappc_b64 s[4:5], s[6:7]
    let e = build(
        &code(&[0xbe842106, 0xbf810000]),
        0,
        false,
        &fetch_regs().with_sgprs(6..8),
    )
    .unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
}

#[test]
fn other_register_jumps_are_typed_errors() {
    // s_rfe_b64 s[8:9] (op 34), and s_call_b64 s[8:9], 0
    let e = build(&code(&[0xbe802208, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
    let e = build(&code(&[0xbb080000, 0xbf810000]), 0, false, &regs()).unwrap_err();
    assert_eq!(e.kind, IrErrorKind::Indirect);
}

#[test]
fn constant_taken_branch_becomes_a_jump_and_the_dead_arm_goes() {
    // s_mov_b32 s0, 1 ; s_cmp_eq_u32 s0, 1 ; s_cbranch_scc1 +1 ; v_mov_b32 v2, v1 ;
    // L: v_add_f32 v3, v2, v2 ; s_endpgm
    let f = ir(&[
        0xbe800381, 0xbf008100, 0xbf850001, 0x7e040301, 0x06060502, 0xbf810000,
    ]);
    check(
        &f,
        "wave64
         %0:v2 = arg
         %1:v3 = arg
         %2:exec_lo = arg
         %3:exec_hi = arg
         b0: ; +0x0 preds []
           jump b1
         b1: ; +0x10 preds [b0]
           %4:v3 = v_add_f32 %0, %0 ; exec_lo=%2, exec_hi=%3, v3=%1
           s_endpgm",
    );
    // v1 was read only by the dead arm: no longer an argument.
    assert!(f.args.iter().all(|(r, _)| *r != Reg::V(1)));
    assert_eq!(f.idom, vec![None, Some(0)]);
    assert_eq!(f.rpo, vec![0, 1]);
    assert_eq!(f.blocks[1].preds, vec![0]);
}

#[test]
fn constant_fallthrough_branch_drops_the_taken_arm_and_its_phi_input() {
    // s_mov_b32 s0, 1 ; s_cmp_eq_u32 s0, 2 ; s_cbranch_scc1 +2 ; v_mov_b32 v2, 1.0 ;
    // s_branch +1 ; v_mov_b32 v2, 0 ; J: v_add_f32 v3, v2, v2 ; s_endpgm
    let f = ir(&[
        0xbe800381, 0xbf008200, 0xbf850002, 0x7e0402f2, 0xbf820001, 0x7e040280, 0x06060502,
        0xbf810000,
    ]);
    check(
        &f,
        "wave64
         %0:v2 = arg
         %1:v3 = arg
         %2:exec_lo = arg
         %3:exec_hi = arg
         b0: ; +0x0 preds []
           jump b1
         b1: ; +0xc preds [b0]
           %4:v2 = v_mov_b32 #0x3f800000 ; exec_lo=%2, exec_hi=%3, v2=%0
           jump b2
         b2: ; +0x18 preds [b1]
           %5:v3 = v_add_f32 %4, %4 ; exec_lo=%2, exec_hi=%3, v3=%1
           s_endpgm",
    );
    assert_eq!(f.idom, vec![None, Some(0), Some(1)]);
}

#[test]
fn branches_on_runtime_conditions_are_kept() {
    // v_cmp_lt_f32 vcc, v0, v1 ; s_cbranch_vccz +1 ; v_mov_b32 v2, 0 ; s_endpgm
    let f = ir(&[0x7c020300, 0xbf86_0001, 0x7e040280, 0xbf810000]);
    assert_eq!(f.blocks.len(), 3);
    assert!(matches!(f.blocks[0].term, Term::Branch { .. }));
    // A wave64 mask with one half a non-zero constant is decided; with the other half
    // unknown and this one zero it is not.
    let nz = [(Reg::VccLo, Src::Const(0)), (Reg::VccHi, Src::Const(2))];
    assert_eq!(constant_branch("s_cbranch_vccnz", &nz), Some(true));
    assert_eq!(constant_branch("s_cbranch_vccz", &nz), Some(false));
    let part = [(Reg::VccLo, Src::Const(0)), (Reg::VccHi, Src::Undef)];
    assert_eq!(constant_branch("s_cbranch_vccnz", &part), None);
    let zero = [(Reg::ExecLo, Src::Const(0)), (Reg::ExecHi, Src::Const(0))];
    assert_eq!(constant_branch("s_cbranch_execz", &zero), Some(true));
    assert_eq!(constant_branch("s_cbranch_execnz", &zero), Some(false));
}

#[test]
fn pruning_a_loop_exit_keeps_the_loop_and_recomputes_dominators() {
    // s_mov_b32 s0, 0 ; L: v_add_f32 v2, v2, v2 ; s_cmp_eq_u32 s0, 0 ; s_cbranch_scc0 X ;
    // s_branch L ; X: v_mov_b32 v3, 0 ; s_endpgm
    // The exit is never taken: X goes, the loop stays, its only edge out is gone.
    let f = ir(&[
        0xbe800380, 0x06040502, 0xbf008000, 0xbf840001, 0xbf82fffc, 0x7e060280, 0xbf810000,
    ]);
    assert_eq!(f.blocks.len(), 3);
    assert_eq!(f.blocks[1].term, Term::Jump(2));
    assert_eq!(f.blocks[2].term, Term::Jump(1));
    assert_eq!(f.blocks[1].preds, vec![0, 2]);
    assert_eq!(f.idom, vec![None, Some(0), Some(1)]);
}

fn record_of(payload: &[u8]) -> crate::shader::native::NativeProgram {
    crate::shader::native::NativeProgram::parse(&crate::shader::native::tests::xbch(payload))
        .unwrap()
}

fn has_all(r: &EntryRegs, regs: impl IntoIterator<Item = Reg>) -> Vec<Reg> {
    regs.into_iter().filter(|x| !r.has(*x)).collect()
}

#[test]
fn entry_regs_come_from_the_stage_data() {
    use crate::shader::native::tests::payload_all;
    let n = record_of(&payload_all(true, true, true, true));
    let (s, v) = (Reg::S, Reg::V);
    // VS: user SGPRs s8..s10 (layout: first dword 8, 2 SGPRs); v0, v2, v3; attributes from
    // the fetch call (first VGPR 4, channels 0 and 1).
    let r = EntryRegs::from_stage(n.vs.as_ref().unwrap());
    assert_eq!(has_all(&r, [s(8), s(9), v(0), v(2), v(3)]), []);
    assert!(![s(3), s(4), s(10), v(1), v(4), v(5)]
        .iter()
        .any(|x| r.has(*x)));
    assert_eq!(r.fetch, Some(vec![4, 5]));
    // PS: user SGPRs, then the primitive mask and the ordering id; SPI_PS_INPUT_ADDR = 2 is
    // the centre barycentrics, v0 and v1.
    let r = EntryRegs::from_stage(n.ps.as_ref().unwrap());
    assert_eq!(has_all(&r, [s(8), s(9), s(10), s(11), v(0), v(1)]), []);
    assert!(![s(7), s(12), v(2)].iter().any(|x| r.has(*x)));
    assert_eq!(r.fetch, None);
    // CS: RSRC2 has 6 user SGPRs, group ids x and y, the group info and the scratch offset
    // after them (s6..s9), two thread-id VGPRs.
    let r = EntryRegs::from_stage(n.cs.as_ref().unwrap());
    assert_eq!(has_all(&r, (0..10).map(s).chain([v(0), v(1)])), []);
    assert!(![s(10), v(2)].iter().any(|x| r.has(*x)));
    // Mesh: s3, five user SGPRs from s8, v5.
    let r = EntryRegs::from_stage(n.mesh.as_ref().unwrap());
    assert_eq!(has_all(&r, [s(3), s(8), s(12), v(5)]), []);
    assert!(![s(0), s(2), s(4), s(7), s(13), v(0), v(4), v(6)]
        .iter()
        .any(|x| r.has(*x)));
    // Every stage has EXEC.
    assert!(r.has(Reg::ExecLo) && r.has(Reg::ExecHi) && !r.has(Reg::VccLo));
}

#[test]
fn entry_regs_follow_the_enabled_system_values() {
    use crate::shader::native::{tests::payload_all, StageData};
    let n = record_of(&payload_all(false, true, true, false));
    let cs_with = |rsrc2: u32| {
        let mut cs = n.cs.clone().unwrap();
        let StageData::Compute(d) = &mut cs.data else {
            unreachable!()
        };
        d.rsrc2 = rsrc2;
        EntryRegs::from_stage(&cs)
    };
    // No scratch, no ids, no info: only the user SGPRs.
    let r = cs_with(2 << 1);
    assert_eq!(r.sgprs, vec![0..2]);
    assert_eq!(r.vgprs, vec![0..1]);
    assert!(cs_with(0).sgprs.is_empty());
    // z id and scratch only, after 1 user SGPR: s1 and s2.
    assert_eq!(cs_with(1 | 1 << 1 | 0x200).sgprs, vec![0..3]);
    // A pixel shader with every input bit: 15 barycentric dwords and 9 system ones.
    let mut ps = n.ps.clone().unwrap();
    let StageData::Pixel(d) = &mut ps.data else {
        unreachable!()
    };
    d.spi_ps_input_addr = 0xffff;
    let r = EntryRegs::from_stage(&ps);
    assert!(r.has(Reg::V(23)) && !r.has(Reg::V(24)));
    // A VS without layout entries has no user SGPRs but still the 8 reserved ones.
    let n = record_of(&payload_all(true, false, false, false));
    let mut vs = n.vs.clone().unwrap();
    vs.layout.clear();
    let r = EntryRegs::from_stage(&vs);
    assert!(r.sgprs.iter().all(Range::is_empty));
}

/// `NATIV64_SHADER_CORPUS=<dir of .dxbc blobs>`: builds the IR of every program with a decoded
/// record, as `nativ64-shader --ir --stats` does, and reports the successes and the errors by
/// kind. Fails on a read of a register the entry table does not explain or a jump through a
/// register: those are the two things the entry and flow cleanup must leave none of. Skipped
/// when the variable is unset.
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
    let (mut programs, mut built, mut instrs, mut phis) = (0, 0, 0, 0);
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut unexplained = Vec::new();
    for f in &files {
        let blob = std::fs::read(f).unwrap();
        let Ok(p) = crate::shader::parse(&blob) else {
            continue;
        };
        for (slot, rec) in p.native.records() {
            programs += 1;
            let regs = EntryRegs::from_stage(rec);
            match build(p.native.code(rec), gfx10::ENTRY, rec.wave32(), &regs) {
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
                    if matches!(
                        e.kind,
                        IrErrorKind::UndefinedRead(_) | IrErrorKind::Indirect
                    ) {
                        unexplained.push(format!("{slot} {f:?}: {e}"));
                    }
                    *errors.entry(format!("{slot}: {key}")).or_default() += 1;
                }
            }
        }
    }
    eprintln!("corpus ir: {built}/{programs} programs built ({instrs} instructions, {phis} phis)");
    let mut v: Vec<_> = errors.into_iter().collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (k, n) in v.iter().take(30) {
        eprintln!("{n:6}  {k}");
    }
    assert!(unexplained.is_empty(), "{unexplained:#?}");
}

fn mem_regs() -> EntryRegs {
    EntryRegs::new().with_sgprs(0..16).with_vgprs(0..16)
}

/// One memory or packed instruction followed by `s_endpgm`.
fn one(words: &[u32]) -> Func {
    let mut w = words.to_vec();
    w.push(0xbf810000);
    build(&code(&w), 0, false, &mem_regs()).unwrap()
}

fn one_err(words: &[u32]) -> IrError {
    let mut w = words.to_vec();
    w.push(0xbf810000);
    build(&code(&w), 0, false, &mem_regs()).unwrap_err()
}

/// The printed instructions of the program, without the entry values and `s_endpgm`.
fn body(f: &Func) -> Vec<String> {
    f.to_string()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| {
            !(l.is_empty()
                || l.contains("= arg")
                || l.starts_with("wave")
                || l.starts_with("b0:")
                || l.starts_with("s_endpgm"))
        })
        .collect()
}

#[track_caller]
fn expect(words: &[u32], want: &str) {
    let got = body(&one(words));
    assert_eq!(got, vec![want.to_string()], "got:\n{got:#?}");
}

fn reads(f: &Func, r: Reg) -> bool {
    f.args.iter().any(|(a, _)| *a == r)
}

#[test]
fn image_loads_and_samples_write_the_data_dwords() {
    // image_sample v[0:3], v[4:5], s[8:15], s[0:3] dmask:0xf dim:2D
    expect(
        &[0xf0800f08, 0x00020004],
        "%20:v0, %21:v1, %22:v2, %23:v3 = image_sample {%16, %17}, {%4, %5, %6, %7, %8, %9, %10, %11}, {%0, %1, %2, %3} ; exec_lo=%18, exec_hi=%19, v0=%12, v1=%13, v2=%14, v3=%15",
    );
    // the same with dmask:0x7 d16: three channels pack into two dwords
    expect(
        &[0xf0800708, 0x80020004],
        "%18:v0, %19:v1 = image_sample {%14, %15}, {%4, %5, %6, %7, %8, %9, %10, %11}, {%0, %1, %2, %3} ; exec_lo=%16, exec_hi=%17, v0=%12, v1=%13",
    );
    // image_load v[0:4], v[4:5], s[8:15] dmask:0xf tfe: the fifth dword is the fail flag
    expect(
        &[0xf0010f08, 0x00020004],
        "%16:v0, %17:v1, %18:v2, %19:v3, %20:v4 = image_load {%12, %13}, {%0, %1, %2, %3, %4, %5, %6, %7} ; exec_lo=%14, exec_hi=%15, v0=%8, v1=%9, v2=%10, v3=%11, v4=%12",
    );
}

#[test]
fn non_sequential_address_lists_name_each_register() {
    // image_sample v[0:3], [v4, v7], s[8:15], s[0:3]
    let f = one(&[0xf0800f0a, 0x00020004, 0x00000007]);
    assert!(reads(&f, Reg::V(7)) && reads(&f, Reg::V(4)), "{f}");
    assert!(!reads(&f, Reg::V(5)), "{f}");
    // image_bvh_intersect_ray v[0:3], [v4..v14], s[8:11]: eleven addresses, a 4-dword resource
    let f = one(&[0xf1989f07, 0x00020004, 0x08070605, 0x0c0b0a09, 0x00000e0d]);
    let t = f.to_string();
    assert!(t.contains("= image_bvh_intersect_ray"), "{t}");
    assert!(reads(&f, Reg::V(14)) && !reads(&f, Reg::V(15)), "{t}");
    assert!(reads(&f, Reg::S(11)) && !reads(&f, Reg::S(12)), "{t}");
}

#[test]
fn image_stores_and_atomics() {
    // image_store v[0:3], v[4:5], s[8:15] dmask:0xf: no result
    expect(
        &[0xf0200f08, 0x00020004],
        "image_store {%8, %9, %10, %11}, {%12, %13}, {%0, %1, %2, %3, %4, %5, %6, %7} ; exec_lo=%14, exec_hi=%15",
    );
    // image_atomic_add v1, ... glc returns the old value; without glc nothing is written
    expect(
        &[0xf0442108, 0x00020104],
        "%13:v1 = image_atomic_add %8, {%9, %10}, {%0, %1, %2, %3, %4, %5, %6, %7} ; exec_lo=%11, exec_hi=%12, v1=%8",
    );
    expect(
        &[0xf0440108, 0x00020104],
        "image_atomic_add %8, {%9, %10}, {%0, %1, %2, %3, %4, %5, %6, %7} ; exec_lo=%11, exec_hi=%12",
    );
    // image_atomic_cmpswap v[1:2] glc: two operands, one result
    expect(
        &[0xf0402308, 0x00020104],
        "%14:v1 = image_atomic_cmpswap {%8, %9}, {%10, %11}, {%0, %1, %2, %3, %4, %5, %6, %7} ; exec_lo=%12, exec_hi=%13, v1=%8",
    );
    // a store with tfe is not modelled: image_store v[0:4] ... tfe
    let e = one_err(&[0xf0210f08, 0x00020004]);
    assert!(matches!(e.kind, IrErrorKind::Unsupported(_)), "{e}");
}

#[test]
fn typed_buffer_loads_and_stores() {
    // tbuffer_load_format_xyzw v[0:3], v4, s[8:11], 0 idxen
    expect(
        &[0xea6b2000, 0x80020004],
        "%11:v0, %12:v1, %13:v2, %14:v3 = tbuffer_load_format_xyzw %8, {%0, %1, %2, %3}, #0x0 ; exec_lo=%9, exec_hi=%10, v0=%4, v1=%5, v2=%6, v3=%7",
    );
    // tbuffer_store_format_d16_xyz v[0:1], v4, s[8:11], 0 offen: two data dwords
    expect(
        &[0xea3e1000, 0x80220004],
        "tbuffer_store_format_d16_xyz {%4, %5}, %6, {%0, %1, %2, %3}, #0x0 ; exec_lo=%7, exec_hi=%8",
    );
}

#[test]
fn ds_reads_writes_and_atomics_need_no_m0_on_lds() {
    // ds_read_b32 v0, v1
    expect(
        &[0xd8d80000, 0x00000001],
        "%4:v0 = ds_read_b32 %1 ; exec_lo=%2, exec_hi=%3, v0=%0",
    );
    // ds_write2_b32 v1, v2, v3 offset1:1
    expect(
        &[0xd8380100, 0x00030201],
        "ds_write2_b32 %0, %1, %2 ; exec_lo=%3, exec_hi=%4",
    );
    // ds_add_rtn_u32 v0, v1, v2 returns, ds_add_u32 v1, v2 does not
    expect(
        &[0xd8800000, 0x00000201],
        "%5:v0 = ds_add_rtn_u32 %1, %2 ; exec_lo=%3, exec_hi=%4, v0=%0",
    );
    expect(
        &[0xd8000000, 0x00000201],
        "ds_add_u32 %0, %1 ; exec_lo=%2, exec_hi=%3",
    );
}

#[test]
fn ds_gds_append_and_addtid_read_m0_and_gws_is_refused() {
    // ds_append v0, ds_write_addtid_b32 v1, ds_add_u32 v1, v2 gds
    for w in [
        &[0xd8f80000u32, 0][..],
        &[0xdac00000, 0x00000100],
        &[0xd8020000, 0x00000201],
    ] {
        let e = one_err(w);
        assert_eq!(e.kind, IrErrorKind::UndefinedRead(Reg::M0), "{e}");
        // with m0 set first (s_mov_b32 m0, 0) the program builds
        let mut p = vec![0xbefc0380];
        p.extend(w);
        p.push(0xbf810000);
        let f = build(&code(&p), 0, false, &mem_regs()).unwrap();
        assert!(f.to_string().contains("ds_"), "{f}");
    }
    // ds_gws_init v1 gds
    let e = one_err(&[0xd8660000, 0x00000001]);
    assert!(matches!(e.kind, IrErrorKind::Unsupported(_)), "{e}");
}

#[test]
fn packed_math_reads_its_sources_and_a_literal() {
    // v_pk_add_f16 v1, v4, v5
    expect(
        &[0xcc0f4001, 0x1a020b04],
        "%5:v1 = v_pk_add_f16 %1, %2 ; exec_lo=%3, exec_hi=%4, v1=%0",
    );
    // v_pk_add_f16 v1, v4, 0x40004000
    expect(
        &[0xcc0f4001, 0x1a01ff04, 0x40004000],
        "%4:v1 = v_pk_add_f16 %1, #0x40004000 ; exec_lo=%2, exec_hi=%3, v1=%0",
    );
}

#[test]
fn flat_global_and_scratch_forms() {
    // global_load_dword v1, v[4:5], off
    expect(
        &[0xdc308000, 0x017d0004],
        "%5:v1 = global_load_dword {%1, %2} ; exec_lo=%3, exec_hi=%4, v1=%0",
    );
    // global_store_dword v[4:5], v1, off
    expect(
        &[0xdc708000, 0x007d0104],
        "global_store_dword {%1, %2}, %0 ; exec_lo=%3, exec_hi=%4",
    );
    // global_store_dword v4, v1, s[8:9]: a 32-bit offset and the scalar base
    expect(
        &[0xdc708000, 0x00080104],
        "global_store_dword %3, %2, {%0, %1} ; exec_lo=%4, exec_hi=%5",
    );
    // global_atomic_add v1, v[4:5], v2, off glc returns; without glc it does not
    expect(
        &[0xdcc98000, 0x017d0204],
        "%6:v1 = global_atomic_add {%2, %3}, %1 ; exec_lo=%4, exec_hi=%5, v1=%0",
    );
    expect(
        &[0xdcc88000, 0x007d0204],
        "global_atomic_add {%1, %2}, %0 ; exec_lo=%3, exec_hi=%4",
    );
    // global_atomic_cmpswap v1, v[4:5], v[2:3], off glc: two operands, one result
    expect(
        &[0xdcc58000, 0x017d0204],
        "%7:v1 = global_atomic_cmpswap {%3, %4}, {%1, %2} ; exec_lo=%5, exec_hi=%6, v1=%0",
    );
    // scratch_load_dword v1, off, s2: the scalar offset is the only address
    expect(
        &[0xdc304000, 0x01020000],
        "%4:v1 = scratch_load_dword %0 ; exec_lo=%2, exec_hi=%3, v1=%1",
    );
}

#[test]
fn memory_forms_the_ir_cannot_model_are_typed_errors() {
    // a 128-bit resource on a sample is not modelled (the decoder reads 8 dwords)
    // image_sample v[0:3], v[4:5], s[8:15], s[0:3] dmask:0xf r128
    let e = one_err(&[0xf0808f08, 0x00020004]);
    assert!(
        matches!(&e.kind, IrErrorKind::Unsupported(s) if s.contains("r128")),
        "{e}"
    );
}
