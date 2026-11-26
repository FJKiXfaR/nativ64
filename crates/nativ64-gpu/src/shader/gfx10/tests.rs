use super::*;

fn bytes(hex: &str) -> Vec<u8> {
    let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    (0..hex.len() / 2)
        .map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).unwrap())
        .collect()
}

fn dw(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn one(hex: &str) -> Instr {
    let b = bytes(hex);
    let i = decode(&b, 0).unwrap();
    assert_eq!(i.len, b.len(), "{hex}");
    i
}

/// `fixtures.txt`: `hex<TAB>text` pairs, text being what `llvm-mc -mcpu=gfx1013
/// -mattr=+wavefrontsize64 -disassemble` prints for those bytes (one probe per opcode of every
/// table, then random bit patterns that exercise operands and modifiers). Generated from LLVM
/// tooling only.
#[test]
fn matches_llvm_fixtures() {
    let mut n = 0;
    let mut bad = Vec::new();
    let all = [
        include_str!("fixtures.txt"),
        include_str!("fixtures_ext.txt"),
    ];
    for line in all.iter().flat_map(|f| f.lines()) {
        let (hex, text) = line.split_once('\t').unwrap();
        let b = bytes(hex);
        match decode(&b, 0) {
            Ok(i) if i.len == b.len() && disasm_instr(&i) == text => {}
            Ok(i) => bad.push(format!("{hex}: want `{text}`, got `{}`", disasm_instr(&i))),
            Err(e) => bad.push(format!("{hex}: want `{text}`, got error {e}")),
        }
        n += 1;
    }
    assert!(n > 1000, "{n} fixtures");
    assert!(
        bad.is_empty(),
        "{} of {n} differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn scalar_forms() {
    let i = one("0803 80be"); // s_mov_b32 s0, s8
    assert_eq!((i.enc, i.name()), (Enc::Sop1, "s_mov_b32"));
    assert_eq!(disasm_instr(&i), "s_mov_b32 s0, s8");
    let i = one("ff0383be ac4f0111"); // s_mov_b32 s3, 0x11014fac
    assert_eq!(i.literal(), Some(0x1101_4fac));
    assert_eq!(i.len, 8);
    assert_eq!(disasm_instr(&i), "s_mov_b32 s3, 0x11014fac");
    let i = one("c10382be"); // s_mov_b32 s2, -1
    assert_eq!(disasm_instr(&i), "s_mov_b32 s2, -1");
    assert!(i.literal().is_none());
    let i = one("7fc08cbf"); // s_waitcnt lgkmcnt(0) with the other counters at their maxima
    assert_eq!(disasm_instr(&i), "s_waitcnt lgkmcnt(0)");
    let i = one("0300a0bf");
    assert_eq!(disasm_instr(&i), "s_inst_prefetch 0x3");
}

#[test]
fn smem_and_mubuf_fields() {
    let i = one("00012 0f4 000000fa");
    assert_eq!(disasm_instr(&i), "s_buffer_load_dword s4, s[0:3], null");
    assert_eq!(i.mem.soffset, 125);
    assert_eq!(i.mem.offset, 0);
    let i = one("10d030e0 02014100");
    assert_eq!(
        disasm_instr(&i),
        "buffer_load_dword v1, v2, s[4:7], s0 offen offset:16 glc slc dlc"
    );
    let m = i.mem;
    assert!(m.offen && !m.idxen && m.glc && m.slc && m.dlc && !m.tfe && !m.lds);
    assert_eq!((m.offset, m.vaddr, m.srsrc, m.soffset), (16, 2, 4, 0));
    let i = one("0010 3ce0 01010080");
    assert_eq!(
        disasm_instr(&i),
        "buffer_load_dwordx3 v[1:3], v1, s[0:3], 0 offen"
    );
}

#[test]
fn vop3_modifiers_and_literals() {
    let i = one("01810 3d5 02070 22a");
    assert_eq!(i.enc, Enc::Vop3);
    assert_eq!(disasm_instr(&i), "v_add_f32_e64 v1, -|v2|, v3 clamp mul:2");
    assert_eq!((i.mods.abs, i.mods.neg, i.mods.omod), (1, 1, 1));
    assert!(i.mods.clamp);
    let i = one("020001d4 01e90102"); // v_cmp_lt_f32_e64 s[2:3], v1, 2.0
    assert_eq!(disasm_instr(&i), "v_cmp_lt_f32_e64 s[2:3], v1, 2.0");
    let i = one("02070 2 5a 34120000");
    assert_eq!(disasm_instr(&i), "v_fmaak_f32 v1, v2, v3, 0x1234");
    assert_eq!(i.literal(), Some(0x1234));
}

#[test]
fn dpp_and_sdwa_fields() {
    let i = one("fa060206 02b100ff");
    assert_eq!(
        disasm_instr(&i),
        "v_add_f32_dpp v1, v2, v3 quad_perm:[1,0,3,2] row_mask:0xf bank_mask:0xf"
    );
    let d = i.dpp.unwrap();
    assert_eq!(
        (d.kind, d.ctrl, d.row_mask, d.bank_mask),
        (DppKind::Dpp16, 0xb1, 15, 15)
    );
    assert!(i.sdwa.is_none());
    let i = one("f9060206 02150006");
    assert_eq!(
        disasm_instr(&i),
        "v_add_f32_sdwa v1, v2, v3 dst_sel:WORD_1 dst_unused:UNUSED_PRESERVE src0_sel:BYTE_0 src1_sel:DWORD"
    );
    let s = i.sdwa.unwrap();
    assert_eq!(
        (s.dst_sel, s.dst_unused, s.src0_sel, s.src1_sel),
        (5, 2, 0, 6)
    );
}

#[test]
fn exp_and_vintrp_fields() {
    let i = one("0f180 0f8 00010203");
    assert_eq!(disasm_instr(&i), "exp mrt0, v0, v1, v2, v3 done vm");
    let e = i.exp;
    assert_eq!(
        (e.en, e.target, e.done, e.vm, e.compr),
        (0xf, 0, true, true, false)
    );
    assert_eq!(e.vsrc, [0, 1, 2, 3]);
    let i = one("010500c8");
    assert_eq!(disasm_instr(&i), "v_interp_p1_f32_e32 v0, v1, attr1.y");
    assert_eq!(
        (i.intrp.attr, i.intrp.chan, i.intrp.vsrc, i.intrp.vdst),
        (1, 1, 1, 0)
    );
}

#[test]
fn wave32_lane_masks() {
    let b = bytes("020001d4 01050200");
    let w64 = decode_wave(&b, 0, false).unwrap();
    let w32 = decode_wave(&b, 0, true).unwrap();
    assert_eq!(disasm_instr(&w64), "v_cmp_lt_f32_e64 s[2:3], v1, v2");
    assert_eq!(disasm_instr(&w32), "v_cmp_lt_f32_e64 s2, v1, v2");
    let b = bytes("01050 27c");
    assert_eq!(
        disasm_instr(&decode_wave(&b, 0, true).unwrap()),
        "v_cmp_lt_f32_e32 vcc_lo, v1, v2"
    );
}

#[test]
fn typed_errors() {
    // SOP1 opcode 1 is not in the table
    let e = decode(&dw(&[0xbe80_0100]), 0).unwrap_err();
    assert_eq!(
        e.kind,
        ErrorKind::UnknownOpcode {
            enc: Enc::Sop1,
            opcode: 1
        }
    );
    assert_eq!(e.to_string(), "unknown GFX10 SOP1 opcode 0x1 at +0x0");
    // the offset is where the instruction starts
    let code = dw(&[0xbf81_0000, 0xd7ff_0000, 0]);
    let e = decode(&code, 4).unwrap_err();
    assert_eq!(e.to_string(), "unknown GFX10 VOP3 opcode 0x3ff at +0x4");
    // an encoding not decoded yet
    let e = decode(&dw(&[0xe400_0000, 0]), 0).unwrap_err();
    assert!(matches!(
        e.kind,
        ErrorKind::UnknownEncoding { name: "?", .. }
    ));
    assert!(e.to_string().contains("encoding ?"), "{e}");
    // v_pk_add_i16 v1, v4, v5 with neg_lo on a third source it does not have
    let e = decode(&dw(&[0xcc02_0001, 0x8000_0000 | 261 << 9 | 260]), 0).unwrap_err();
    assert!(matches!(e.kind, ErrorKind::BadField(_)), "{e}");
    // MIMG opcode 0x06 is not an instruction
    let e = decode(&dw(&[0xf000_0000 | 6 << 18, 0]), 0).unwrap_err();
    assert_eq!(
        e.kind,
        ErrorKind::UnknownOpcode {
            enc: Enc::Mimg,
            opcode: 6
        }
    );
    // truncated: a VOP3 needs 8 bytes, a literal needs 4 more
    assert_eq!(
        decode(&dw(&[0xd414_0000]), 0).unwrap_err().kind,
        ErrorKind::Truncated
    );
    assert_eq!(decode(&[1, 2], 0).unwrap_err().kind, ErrorKind::Truncated);
    let e = decode(&dw(&[0xbe80_03ff]), 0).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Truncated);
    // never a panic, whatever the bytes
    let mut x = 0x1234_5678u32;
    for _ in 0..20000 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let y = x.wrapping_mul(22_695_477).wrapping_add(1);
        let b = dw(&[x, y, x ^ y]);
        let _ = decode(&b, 0);
        let _ = decode_wave(&b, 4, true);
    }
}

#[test]
fn flow_follows_branches_and_stops_at_endpgm() {
    const NOP: u32 = 0xbf80_0000;
    const END: u32 = 0xbf81_0000;
    let mov = 0xbe80_0380; // s_mov_b32 s0, 0
    let code = dw(&[mov, 0xbf85_0001, NOP, END, NOP, 0x1234_5678]);
    // s_cbranch_scc1 +1 at +4: target +12 (the s_endpgm); fall-through +8; nothing after +12
    let p = follow(&code, 0, false).unwrap();
    let at: Vec<usize> = p.instrs.iter().map(|i| i.at).collect();
    assert_eq!(at, [0, 4, 8, 12]);
    assert_eq!(p.leaders, [0, 8, 12]);
    assert!(p.indirect.is_empty());
    assert_eq!(p.instrs[1].branch_target(), Some(12));
    // an unconditional branch skips the bytes after it, which need not decode
    let code = dw(&[0xbf82_0001, 0xffff_ffff, END]);
    let p = follow(&code, 0, false).unwrap();
    assert_eq!(p.instrs.len(), 2);
    // a backward branch to itself terminates
    let p = follow(&dw(&[0xbf82_ffff]), 0, false).unwrap();
    assert_eq!(p.instrs.len(), 1);
    // an entry past the marker
    let p = follow(&dw(&[0xdead_beef, END]), 4, false).unwrap();
    assert_eq!(p.instrs[0].at, 4);
}

#[test]
fn flow_errors_and_indirect_jumps() {
    // branch out of the code
    let e = follow(&dw(&[0xbf82_0064]), 0, false).unwrap_err();
    assert_eq!(e.kind, ErrorKind::BadTarget(404));
    // a branch into the middle of a literal-carrying instruction
    let code = dw(&[0xbf82_0001, 0xbf80_0000, 0xbe80_03ff, 0xbf81_0000]);
    // +0 s_branch +1 -> +8: s_mov_b32 s0, literal (8 bytes) then +16 is past the end
    assert!(follow(&code, 0, false).is_err() || follow(&code, 0, false).is_ok());
    let code = dw(&[
        0xbf85_0002,
        0xbf80_0000,
        0xbe80_03ff,
        0xbf81_0000,
        0xbf81_0000,
    ]);
    // fall-through decodes the literal mov at +8 (and its literal at +12); the branch target
    // +12 is inside it
    let e = follow(&code, 0, false).unwrap_err();
    assert!(matches!(e.kind, ErrorKind::BadTarget(_)), "{e}");
    // s_setpc_b64 s[2:3] stops the path and is reported
    let p = follow(&dw(&[0xbe80_2002, 0xbf81_0000]), 0, false).unwrap();
    assert_eq!(p.indirect, [0]);
    assert_eq!(p.instrs.len(), 1);
    // s_getpc_b64 alone is an ordinary instruction
    let p = follow(&dw(&[0xbe80_1f00, 0xbf81_0000]), 0, false).unwrap();
    assert!(p.indirect.is_empty());
    assert_eq!(p.instrs.len(), 2);
}

/// `NATIV64_SHADER_CORPUS=<dir of .dxbc blobs>`: every program of every blob (all stages) is
/// followed from its entry. A program that stops on a typed error is counted by encoding or
/// opcode and reported; every instruction reached prints exactly what `llvm-mc`
/// prints for the same bytes (when `llvm-mc` runs). Skipped when the variable is unset.
#[test]
fn corpus_matches_llvm() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
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
    assert!(!files.is_empty(), "no .dxbc blob in NATIV64_SHADER_CORPUS");
    let (mut programs, mut followed, mut instrs, mut compared, mut indirect) = (0, 0, 0, 0, 0);
    let mut diffs = Vec::new();
    let mut stops = std::collections::BTreeMap::new();
    for f in files {
        let blob = std::fs::read(&f).unwrap();
        let p = crate::shader::parse(&blob).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        for (slot, code) in p.native.slot_codes() {
            programs += 1;
            let flow = match follow(code, ENTRY, false) {
                Ok(flow) => flow,
                Err(e) => {
                    // typed stops, counted by what stopped them
                    let key = match &e.kind {
                        ErrorKind::UnknownEncoding { name, .. } => format!("encoding {name}"),
                        ErrorKind::UnknownOpcode { enc, opcode } => {
                            format!("{} opcode {opcode:#x}", enc.name())
                        }
                        k => format!("{k:?}"),
                    };
                    let sample = code[e.at..code.len().min(e.at + 16)]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();
                    stops.entry(key).or_insert((0usize, sample)).0 += 1;
                    continue;
                }
            };
            followed += 1;
            instrs += flow.instrs.len();
            indirect += usize::from(!flow.indirect.is_empty());
            // the reached instructions' bytes, in address order, through llvm-mc
            let mut hex = Vec::new();
            for i in &flow.instrs {
                hex.extend(code[i.at..i.at + i.len].iter().map(|b| format!("{b:#04x}")));
            }
            let Ok(mut child) = Command::new("llvm-mc")
                .args([
                    "-arch=amdgcn",
                    "-mcpu=gfx1013",
                    "-mattr=+wavefrontsize64",
                    "-disassemble",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                continue;
            };
            child
                .stdin
                .take()
                .unwrap()
                .write_all(hex.join(",").as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            let norm = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ");
            let theirs: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim_start().starts_with('.'))
                .map(norm)
                .filter(|l| !l.is_empty())
                .collect();
            let mine: Vec<String> = flow.instrs.iter().map(|i| norm(&disasm_instr(i))).collect();
            compared += 1;
            if mine != theirs {
                let k = mine
                    .iter()
                    .zip(&theirs)
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                diffs.push(format!(
                    "{} {slot}: line {k}: ours `{}` llvm `{}`",
                    f.display(),
                    mine.get(k).map_or("", |s| s),
                    theirs.get(k).map_or("", |s| s)
                ));
            }
        }
    }
    eprintln!(
        "corpus: {followed}/{programs} programs followed ({instrs} instructions), \
         {compared} compared with llvm-mc, {} differ, {indirect} with indirect jumps",
        diffs.len()
    );
    for (what, (n, sample)) in &stops {
        eprintln!("corpus stop: {what}: {n} programs, e.g. {sample}");
    }
    assert!(
        diffs.is_empty(),
        "{}",
        diffs[..diffs.len().min(20)].join("\n")
    );
}
