//! `disasm`: LLVM assembler syntax (`llvm-mc -arch=amdgcn -mcpu=gfx1013`), so that tests can
//! diff against LLVM's own output. Branch targets stay numeric, as `llvm-mc -disassemble`
//! prints them.

use super::ops::{
    Slot, FLAG_CLAMP, FLAG_DLC, FLAG_FMODS_SHIFT, FLAG_GLC, FLAG_IMG_BVH, FLAG_LIT16, FLAG_OMOD,
    FLAG_OPSEL, FLAG_SUFFIX,
};
use super::table_ext;
use super::{decode_wave, DecodeError, DppKind, Enc, Instr, Operand};
use std::fmt::Write as _;

/// Decodes `code` from the start and prints one instruction per line (no offsets) until the
/// code ends. Stops with the error of the first undecodable instruction.
pub fn disasm(code: &[u8], wave32: bool) -> Result<String, DecodeError> {
    let mut out = String::new();
    let mut at = 0;
    while at < code.len() {
        let i = decode_wave(code, at, wave32)?;
        out.push_str(&disasm_instr(&i));
        out.push('\n');
        at += i.len;
    }
    Ok(out)
}

/// One instruction in LLVM syntax.
pub fn disasm_instr(i: &Instr) -> String {
    let mut s = String::new();
    s.push_str(i.info.name);
    let suffix = i.info.flags & FLAG_SUFFIX != 0;
    if i.dpp.is_some() {
        s.push_str("_dpp");
    } else if i.sdwa.is_some() {
        s.push_str("_sdwa");
    } else if suffix {
        s.push_str(if i.enc == Enc::Vop3 { "_e64" } else { "_e32" });
    }
    match i.enc {
        Enc::Exp => return exp(i, s),
        Enc::Vintrp => return vintrp(i, s),
        Enc::Mimg => return mimg(i, s),
        _ => {}
    }
    let mut ops: Vec<String> = Vec::new();
    let mut src = 0;
    for (slot, op) in i.info.slots.iter().zip(&i.operands) {
        let is_src = matches!(
            slot,
            Slot::Src0(_) | Slot::Src1(_) | Slot::Src2(_) | Slot::VSrc1(_)
        );
        let idx = match slot {
            Slot::Src0(_) => 0,
            Slot::Src1(_) | Slot::VSrc1(_) => 1,
            _ => 2,
        };
        let mut t = match slot {
            Slot::Attr => format!(
                "attr{}.{}",
                imm(op) & 0x3f,
                ['x', 'y', 'z', 'w'][(imm(op) >> 6 & 3) as usize]
            ),
            Slot::Waitcnt => waitcnt(imm(op)),
            Slot::Sendmsg => sendmsg(imm(op)),
            Slot::Hwreg => hwreg(imm(op)),
            Slot::Endpgm => {
                if imm(op) == 0 {
                    continue;
                }
                imm(op).to_string()
            }
            Slot::Dec16 => imm(op).to_string(),
            Slot::Hex16 | Slot::Version => format!("{:#x}", imm(op)),
            Slot::Imm16 | Slot::SDataImm => {
                if imm(op) <= 64 {
                    imm(op).to_string()
                } else {
                    format!("{:#x}", imm(op))
                }
            }
            Slot::Lit => operand(op, i.wave32),
            _ => match op {
                Operand::Literal(v) if i.info.flags & FLAG_LIT16 != 0 => literal16(*v),
                _ => operand(op, i.wave32),
            },
        };
        if is_src {
            t = source_mods(i, idx, t, op);
            src += 1;
        }
        ops.push(t);
    }
    let _ = src;
    // the f16 interpolations take the attribute's high half from bit 8 of source 0
    let high = i
        .info
        .slots
        .iter()
        .zip(&i.operands)
        .any(|(s, o)| *s == Slot::Attr && imm(o) >> 8 & 1 != 0);
    if high {
        if let Some(last) = ops.last_mut() {
            last.push_str(" high");
        }
    }
    match i.enc {
        Enc::Smem => smem(i, &mut ops),
        Enc::Mubuf => return mubuf(i, s, ops),
        Enc::Mtbuf => return mtbuf(i, s, ops),
        _ => {}
    }
    if !ops.is_empty() {
        s.push(' ');
        s.push_str(&ops.join(", "));
    }
    modifiers(i, &mut s);
    s
}

fn imm(o: &Operand) -> u32 {
    match o {
        Operand::Imm(v) => *v,
        _ => 0,
    }
}

fn operand(o: &Operand, wave32: bool) -> String {
    match o {
        Operand::Reg { code, count } => reg(*code, *count),
        Operand::Literal(v) => literal(*v),
        Operand::Imm(v) => v.to_string(),
        Operand::Fixed(t) => {
            if wave32 && (*t == "vcc" || *t == "exec") {
                format!("{t}_lo")
            } else {
                (*t).to_string()
            }
        }
    }
}

/// A 32-bit literal as LLVM prints it: small integers and the inline float values by name.
fn literal(v: u32) -> String {
    let i = v as i32;
    if (-16..=64).contains(&i) {
        return i.to_string();
    }
    match v {
        0x3f00_0000 => "0.5".into(),
        0xbf00_0000 => "-0.5".into(),
        0x3f80_0000 => "1.0".into(),
        0xbf80_0000 => "-1.0".into(),
        0x4000_0000 => "2.0".into(),
        0xc000_0000 => "-2.0".into(),
        0x4080_0000 => "4.0".into(),
        0xc080_0000 => "-4.0".into(),
        0x3e22_f983 => "0.15915494".into(),
        _ => format!("{v:#x}"),
    }
}

/// A literal of a 16-bit operand: the low half, small integers and half-float constants.
fn literal16(v: u32) -> String {
    let h = v as u16;
    let i = i32::from(h as i16);
    if (-16..=64).contains(&i) {
        return i.to_string();
    }
    match h {
        0x3800 => "0.5".into(),
        0xb800 => "-0.5".into(),
        0x3c00 => "1.0".into(),
        0xbc00 => "-1.0".into(),
        0x4000 => "2.0".into(),
        0xc000 => "-2.0".into(),
        0x4400 => "4.0".into(),
        0xc400 => "-4.0".into(),
        0x3118 => "0.15915494".into(),
        _ => format!("{h:#x}"),
    }
}

/// A register or constant by its source code and width.
pub(super) fn reg(code: u16, count: u8) -> String {
    let c = u32::from(count.max(1));
    // scalar register tuples start at a multiple of their (rounded) width, at most 4
    let n = match code {
        0..=123 if c > 1 => u32::from(code) & !(c.min(4).next_power_of_two() - 1),
        _ => u32::from(code),
    };
    let code = n as u16;
    let range = |p: &str, lo: u32| {
        if c == 1 {
            format!("{p}{lo}")
        } else {
            format!("{p}[{lo}:{}]", lo + c - 1)
        }
    };
    match code {
        0..=105 => range("s", n),
        106 if c == 2 => "vcc".into(),
        106 => "vcc_lo".into(),
        107 => "vcc_hi".into(),
        108..=123 => range("ttmp", n - 108),
        124 => "m0".into(),
        125 => "null".into(),
        126 if c == 2 => "exec".into(),
        126 => "exec_lo".into(),
        127 => "exec_hi".into(),
        128 => "0".into(),
        129..=192 => (n - 128).to_string(),
        193..=208 => format!("-{}", n - 192),
        230 => "src_flat_scratch_base_lo".into(),
        231 => "src_flat_scratch_base_hi".into(),
        235 => "src_shared_base".into(),
        236 => "src_shared_limit".into(),
        237 => "src_private_base".into(),
        238 => "src_private_limit".into(),
        239 => "src_pops_exiting_wave_id".into(),
        240 => "0.5".into(),
        241 => "-0.5".into(),
        242 => "1.0".into(),
        243 => "-1.0".into(),
        244 => "2.0".into(),
        245 => "-2.0".into(),
        246 => "4.0".into(),
        247 => "-4.0".into(),
        248 if c == 2 => "0.15915494309189532".into(),
        248 => "0.15915494".into(),
        251 => "src_vccz".into(),
        252 => "src_execz".into(),
        253 => "src_scc".into(),
        254 => "src_lds_direct".into(),
        256..=511 => range("v", n - 256),
        _ => format!("src{code}"),
    }
}

fn source_mods(i: &Instr, idx: usize, t: String, op: &Operand) -> String {
    let (abs, neg, sext) = if let Some(d) = &i.dpp {
        match idx {
            0 => (d.src0_abs, d.src0_neg, false),
            _ => (d.src1_abs, d.src1_neg, false),
        }
    } else if let Some(d) = &i.sdwa {
        let m = i.info.flags >> (FLAG_FMODS_SHIFT + idx as u32) & 1 != 0;
        match idx {
            0 => (m && d.src0_abs, m && d.src0_neg, d.src0_sext),
            _ => (m && d.src1_abs, m && d.src1_neg, d.src1_sext),
        }
    } else if i.info.flags >> (FLAG_FMODS_SHIFT + idx as u32) & 1 != 0 {
        (
            i.mods.abs >> idx & 1 != 0,
            i.mods.neg >> idx & 1 != 0,
            false,
        )
    } else {
        (false, false, false)
    };
    let mut t = t;
    if sext {
        t = format!("sext({t})");
    }
    if abs {
        t = format!("|{t}|");
    }
    if neg {
        let constant = matches!(
            op,
            Operand::Literal(_) | Operand::Reg {
                code: 128..=208 | 240..=248,
                ..
            }
        );
        t = if constant && !abs {
            format!("neg({t})")
        } else {
            format!("-{t}")
        };
    }
    t
}

fn modifiers(i: &Instr, s: &mut String) {
    let w = |s: &mut String, t: &str| {
        s.push(' ');
        s.push_str(t);
    };
    if let Some(d) = &i.dpp {
        match d.kind {
            DppKind::Dpp16 => {
                w(s, &dpp_ctrl(d.ctrl));
                w(s, &format!("row_mask:{:#x}", d.row_mask));
                w(s, &format!("bank_mask:{:#x}", d.bank_mask));
                if d.bound_ctrl {
                    w(s, "bound_ctrl:1");
                }
                if d.fi {
                    w(s, "fi:1");
                }
            }
            DppKind::Dpp8 | DppKind::Dpp8Fi => {
                let sel: Vec<String> = d.sel.iter().map(|x| x.to_string()).collect();
                w(s, &format!("dpp8:[{}]", sel.join(",")));
                if d.kind == DppKind::Dpp8Fi {
                    w(s, "fi:1");
                }
            }
        }
        return;
    }
    if let Some(d) = &i.sdwa {
        const SEL: [&str; 7] = [
            "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD",
        ];
        const UNUSED: [&str; 3] = ["UNUSED_PAD", "UNUSED_SEXT", "UNUSED_PRESERVE"];
        let sel = |v: u8| SEL.get(v as usize).copied().unwrap_or("DWORD");
        if d.clamp {
            w(s, "clamp");
        }
        match d.omod {
            1 => w(s, "mul:2"),
            2 => w(s, "mul:4"),
            3 => w(s, "div:2"),
            _ => {}
        }
        if i.enc != Enc::Vopc {
            w(s, &format!("dst_sel:{}", sel(d.dst_sel)));
            w(
                s,
                &format!(
                    "dst_unused:{}",
                    UNUSED
                        .get(d.dst_unused as usize)
                        .copied()
                        .unwrap_or("UNUSED_PAD")
                ),
            );
        }
        w(s, &format!("src0_sel:{}", sel(d.src0_sel)));
        if i.enc != Enc::Vop1 {
            w(s, &format!("src1_sel:{}", sel(d.src1_sel)));
        }
        return;
    }
    if i.enc == Enc::Vop3 {
        let n = i
            .info
            .slots
            .iter()
            .filter(|s| matches!(s, Slot::Src0(_) | Slot::Src1(_) | Slot::Src2(_)))
            .count();
        let entries = (i.info.flags >> 12 & 15) as usize;
        if i.info.flags & FLAG_OPSEL != 0 && i.mods.opsel != 0 {
            let bits: Vec<String> = (0..entries)
                .map(|k| (i.mods.opsel >> if k < n { k } else { 3 } & 1).to_string())
                .collect();
            w(s, &format!("op_sel:[{}]", bits.join(",")));
        }
        if i.mods.clamp && i.info.flags & FLAG_CLAMP != 0 {
            w(s, "clamp");
        }
        if i.info.flags & FLAG_OMOD != 0 {
            match i.mods.omod {
                1 => w(s, "mul:2"),
                2 => w(s, "mul:4"),
                3 => w(s, "div:2"),
                _ => {}
            }
        }
    }
}

fn dpp_ctrl(c: u16) -> String {
    match c {
        0..=0xff => format!(
            "quad_perm:[{},{},{},{}]",
            c & 3,
            c >> 2 & 3,
            c >> 4 & 3,
            c >> 6 & 3
        ),
        0x101..=0x10f => format!("row_shl:{}", c & 15),
        0x111..=0x11f => format!("row_shr:{}", c & 15),
        0x121..=0x12f => format!("row_ror:{}", c & 15),
        0x140 => "row_mirror".into(),
        0x141 => "row_half_mirror".into(),
        0x150..=0x15f => format!("row_share:{}", c & 15),
        0x160..=0x16f => format!("row_xmask:{}", c & 15),
        _ => format!("dpp_ctrl:{c:#x}"),
    }
}

fn smem(i: &Instr, ops: &mut Vec<String>) {
    let m = &i.mem;
    let has_base = i.info.slots.iter().any(|s| matches!(s, Slot::SBase(_)));
    if has_base {
        let off = if m.offset < 0 {
            format!("-{:#x}", -(m.offset as i64))
        } else {
            format!("{:#x}", m.offset)
        };
        if m.soffset == 125 {
            ops.push(if m.offset == 0 { "null".into() } else { off });
        } else {
            let mut t = reg(m.soffset, 1);
            if m.offset != 0 {
                t.push_str(" offset:");
                t.push_str(&off);
            }
            ops.push(t);
        }
    }
    // glc / dlc follow the operands as modifiers
    if let Some(last) = ops.last_mut() {
        if m.glc && i.info.flags & FLAG_GLC != 0 {
            last.push_str(" glc");
        }
        if m.dlc && i.info.flags & FLAG_DLC != 0 {
            last.push_str(" dlc");
        }
    }
}

fn mubuf(i: &Instr, mut s: String, ops: Vec<String>) -> String {
    let m = &i.mem;
    let mut parts = ops;
    if m.lds {
        // a load to LDS has no data register
        parts.clear();
    }
    if i.info.slots.is_empty() {
        // no data operand (buffer_gl0_inv): print as the bare name
        return s;
    }
    parts.push(match (m.offen, m.idxen) {
        (false, false) => "off".into(),
        (true, true) => format!("v[{}:{}]", m.vaddr, m.vaddr as u32 + 1),
        _ => format!("v{}", m.vaddr),
    });
    parts.push(reg(m.srsrc as u16, 4));
    parts.push(reg(m.soffset, 1));
    s.push(' ');
    s.push_str(&parts.join(", "));
    if m.idxen {
        s.push_str(" idxen");
    }
    if m.offen {
        s.push_str(" offen");
    }
    if m.offset != 0 {
        let _ = write!(s, " offset:{}", m.offset);
    }
    if m.glc {
        s.push_str(" glc");
    }
    if m.slc {
        s.push_str(" slc");
    }
    if m.dlc {
        s.push_str(" dlc");
    }
    if m.tfe {
        s.push_str(" tfe");
    }
    if m.lds {
        s.push_str(" lds");
    }
    s
}

fn mimg(i: &Instr, mut s: String) -> String {
    const DIM: [&str; 8] = [
        "1D",
        "2D",
        "3D",
        "CUBE",
        "1D_ARRAY",
        "2D_ARRAY",
        "2D_MSAA",
        "2D_MSAA_ARRAY",
    ];
    let m = &i.img;
    let mut parts: Vec<String> = Vec::new();
    for (slot, op) in i.info.slots.iter().zip(&i.operands) {
        parts.push(match slot {
            Slot::ImgAddr if m.nsa > 0 => {
                let regs: Vec<String> = m.vaddr[..usize::from(m.naddr).max(1)]
                    .iter()
                    .map(|r| format!("v{r}"))
                    .collect();
                format!("[{}]", regs.join(", "))
            }
            _ => operand(op, i.wave32),
        });
    }
    if i.info.flags & FLAG_IMG_BVH != 0 {
        return format!("{s} {}", parts.join(", "));
    }
    let _ = write!(
        s,
        " {} dmask:{:#x} dim:SQ_RSRC_IMG_{}",
        parts.join(", "),
        m.dmask,
        DIM[usize::from(m.dim & 7)]
    );
    for (on, t) in [
        (m.unorm, "unorm"),
        (m.glc, "glc"),
        (m.slc, "slc"),
        (m.dlc, "dlc"),
        (m.r128, "r128"),
        (m.a16, "a16"),
        (m.tfe, "tfe"),
        (m.lwe, "lwe"),
        (m.d16, "d16"),
    ] {
        if on {
            s.push(' ');
            s.push_str(t);
        }
    }
    s
}

fn mtbuf(i: &Instr, mut s: String, mut parts: Vec<String>) -> String {
    let m = &i.mem;
    parts.push(match (m.offen, m.idxen) {
        (false, false) => "off".into(),
        (true, true) => format!("v[{}:{}]", m.vaddr, m.vaddr as u32 + 1),
        _ => format!("v{}", m.vaddr),
    });
    parts.push(reg(m.srsrc as u16, 4));
    parts.push(reg(m.soffset, 1));
    s.push(' ');
    s.push_str(&parts.join(", "));
    // the default format (8-bit unorm) is not printed; unnamed values print as numbers
    if m.format != 1 {
        match table_ext::UFMT.get(usize::from(m.format)) {
            Some(n) if !n.is_empty() => {
                let _ = write!(s, " format:[{n}]");
            }
            _ => {
                let _ = write!(s, " format:{}", m.format);
            }
        }
    }
    if m.idxen {
        s.push_str(" idxen");
    }
    if m.offen {
        s.push_str(" offen");
    }
    if m.offset != 0 {
        let _ = write!(s, " offset:{}", m.offset);
    }
    for (on, t) in [(m.glc, "glc"), (m.slc, "slc"), (m.dlc, "dlc")] {
        if on {
            s.push(' ');
            s.push_str(t);
        }
    }
    s
}

fn exp(i: &Instr, mut s: String) -> String {
    let e = &i.exp;
    let target = match e.target {
        0..=7 => format!("mrt{}", e.target),
        8 => "mrtz".into(),
        9 => "null".into(),
        12..=16 => format!("pos{}", e.target - 12),
        20 => "prim".into(),
        32..=63 => format!("param{}", e.target - 32),
        t => format!("invalid_target_{t}"),
    };
    let ch = |k: usize| -> String {
        let (bit, src) = if e.compr {
            (k, if k < 2 { 0 } else { 1 })
        } else {
            (k, k)
        };
        if e.en >> bit & 1 != 0 {
            format!("v{}", e.vsrc[src])
        } else {
            "off".into()
        }
    };
    let _ = write!(s, " {target}, {}, {}, {}, {}", ch(0), ch(1), ch(2), ch(3));
    if e.done {
        s.push_str(" done");
    }
    if e.compr {
        s.push_str(" compr");
    }
    if e.vm {
        s.push_str(" vm");
    }
    s
}

fn vintrp(i: &Instr, mut s: String) -> String {
    let t = &i.intrp;
    let chan = ['x', 'y', 'z', 'w'][t.chan as usize & 3];
    let src = if i.opcode == 2 {
        match t.vsrc {
            0 => "p10".to_string(),
            1 => "p20".to_string(),
            2 => "p0".to_string(),
            n => format!("invalid_param_{n}"),
        }
    } else {
        format!("v{}", t.vsrc)
    };
    let _ = write!(s, " v{}, {src}, attr{}.{chan}", t.vdst, t.attr);
    s
}

fn waitcnt(v: u32) -> String {
    let vm = (v & 0xf) | (v >> 14 & 3) << 4;
    let ex = v >> 4 & 7;
    let lgkm = v >> 8 & 0x3f;
    let (dvm, dex, dlg) = (vm == 63, ex == 7, lgkm == 63);
    let all = dvm && dex && dlg;
    let mut parts = Vec::new();
    if !dvm || all {
        parts.push(format!("vmcnt({vm})"));
    }
    if !dex || all {
        parts.push(format!("expcnt({ex})"));
    }
    if !dlg || all {
        parts.push(format!("lgkmcnt({lgkm})"));
    }
    parts.join(" ")
}

fn sendmsg(v: u32) -> String {
    let (id, op, stream) = (v & 0xf, v >> 4 & 7, v >> 8 & 3);
    const MSG: [&str; 12] = [
        "",
        "MSG_INTERRUPT",
        "MSG_GS",
        "MSG_GS_DONE",
        "MSG_SAVEWAVE",
        "MSG_STALL_WAVE_GEN",
        "MSG_HALT_WAVES",
        "MSG_ORDERED_PS_DONE",
        "MSG_EARLY_PRIM_DEALLOC",
        "MSG_GS_ALLOC_REQ",
        "MSG_GET_DOORBELL",
        "MSG_GET_DDID",
    ];
    const GSOP: [&str; 4] = ["GS_OP_NOP", "GS_OP_CUT", "GS_OP_EMIT", "GS_OP_EMIT_CUT"];
    const SYSOP: [&str; 5] = [
        "",
        "SYSMSG_OP_ECC_ERR_INTERRUPT",
        "SYSMSG_OP_REG_RD",
        "SYSMSG_OP_HOST_TRAP_ACK",
        "SYSMSG_OP_TTRACE_PC",
    ];
    // an unnamed message prints as a tuple only when the fields re-encode to the same value
    let numeric = || {
        if v & !0x37f == 0 {
            format!("sendmsg({id}, {op}, {stream})")
        } else {
            v.to_string()
        }
    };
    match id {
        1 | 4..=11 if op == 0 && stream == 0 => format!("sendmsg({})", MSG[id as usize]),
        2 | 3 if op < 4 => {
            if op == 0 {
                if id == 3 && stream == 0 {
                    format!("sendmsg({}, {})", MSG[3], GSOP[0])
                } else {
                    numeric()
                }
            } else {
                format!(
                    "sendmsg({}, {}, {stream})",
                    MSG[id as usize], GSOP[op as usize]
                )
            }
        }
        15 if (1..=4).contains(&op) && stream == 0 => {
            format!("sendmsg(MSG_SYSMSG, {})", SYSOP[op as usize])
        }
        _ => numeric(),
    }
}

fn hwreg(v: u32) -> String {
    const NAMES: [&str; 32] = [
        "",
        "HW_REG_MODE",
        "HW_REG_STATUS",
        "HW_REG_TRAPSTS",
        "",
        "HW_REG_GPR_ALLOC",
        "HW_REG_LDS_ALLOC",
        "HW_REG_IB_STS",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "HW_REG_SH_MEM_BASES",
        "HW_REG_TBA_LO",
        "HW_REG_TBA_HI",
        "HW_REG_TMA_LO",
        "HW_REG_TMA_HI",
        "HW_REG_FLAT_SCR_LO",
        "HW_REG_FLAT_SCR_HI",
        "HW_REG_XNACK_MASK",
        "HW_REG_HW_ID1",
        "HW_REG_HW_ID2",
        "HW_REG_POPS_PACKER",
        "",
        "",
        "",
        "",
        "",
        "",
    ];
    let id = v & 0x3f;
    let off = v >> 6 & 0x1f;
    let size = (v >> 11 & 0x1f) + 1;
    let name = NAMES.get(id as usize).copied().unwrap_or("");
    let n = if name.is_empty() {
        id.to_string()
    } else {
        name.to_string()
    };
    if !name.is_empty() && off == 0 && size == 32 {
        format!("hwreg({n})")
    } else {
        format!("hwreg({n}, {off}, {size})")
    }
}
