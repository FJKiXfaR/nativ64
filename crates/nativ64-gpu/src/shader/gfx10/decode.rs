//! `decode`: one instruction from machine code.

use super::ops::{OpInfo, Slot};
use super::table;
use super::{
    DecodeError, Dpp, DppKind, Enc, ErrorKind, Exp, Instr, Intrp, Mem, Mods, Operand, Sdwa,
};

/// Decodes the instruction at byte offset `at` of `code` (wave64 operand widths).
pub fn decode(code: &[u8], at: usize) -> Result<Instr, DecodeError> {
    decode_wave(code, at, false)
}

/// Decodes the instruction at byte offset `at`; `wave32` sets the width of lane-mask operands.
pub fn decode_wave(code: &[u8], at: usize, wave32: bool) -> Result<Instr, DecodeError> {
    let c = Cursor { code, at, wave32 };
    let d0 = c.dword(0)?;
    let top6 = d0 >> 26;
    let top9 = d0 >> 23;
    let enc = if d0 >> 31 == 0 {
        match d0 >> 25 {
            0x3f => Enc::Vop1,
            0x3e => Enc::Vopc,
            _ => Enc::Vop2,
        }
    } else if top9 == 0x17d {
        Enc::Sop1
    } else if top9 == 0x17e {
        Enc::Sopc
    } else if top9 == 0x17f {
        Enc::Sopp
    } else if d0 >> 28 == 0xb {
        Enc::Sopk
    } else if d0 >> 30 == 2 {
        Enc::Sop2
    } else {
        match top6 {
            0x32 => Enc::Vintrp,
            0x35 => Enc::Vop3,
            0x38 => Enc::Mubuf,
            0x3d => Enc::Smem,
            0x3e => Enc::Exp,
            _ => {
                let name = match top6 {
                    0x33 => "VOP3P",
                    0x36 => "DS",
                    0x37 => "FLAT",
                    0x3a => "MTBUF",
                    0x3c => "MIMG",
                    _ => "?",
                };
                return Err(c.err(ErrorKind::UnknownEncoding { name, word: d0 }));
            }
        }
    };
    match enc {
        Enc::Sop1 => c.sop1(d0),
        Enc::Sop2 => c.sop2(d0),
        Enc::Sopc => c.sopc(d0),
        Enc::Sopk => c.sopk(d0),
        Enc::Sopp => c.sopp(d0),
        Enc::Smem => c.smem(d0),
        Enc::Vop1 | Enc::Vop2 | Enc::Vopc => c.vop12c(enc, d0),
        Enc::Vop3 => c.vop3(d0),
        Enc::Vintrp => c.vintrp(d0),
        Enc::Exp => c.exp(d0),
        Enc::Mubuf => c.mubuf(d0),
    }
}

/// The raw field values of an instruction, by the names the slots use.
#[derive(Default)]
struct Fields {
    sdst: u32,
    ssrc0: u32,
    ssrc1: u32,
    vdst: u32,
    src0: u32,
    vsrc1: u32,
    src1: u32,
    src2: u32,
    sdstb: u32,
    sbase: u32,
    sdata: u32,
    vdata: u32,
    imm16: u32,
    /// VMEM data dwords added by `tfe`.
    extra: u8,
}

struct Cursor<'a> {
    code: &'a [u8],
    at: usize,
    wave32: bool,
}

impl Cursor<'_> {
    fn err(&self, kind: ErrorKind) -> DecodeError {
        DecodeError { at: self.at, kind }
    }

    /// The `i`-th dword from the instruction start.
    fn dword(&self, i: usize) -> Result<u32, DecodeError> {
        let o = self.at + 4 * i;
        self.code
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| self.err(ErrorKind::Truncated))
    }

    fn lookup(
        &self,
        table: &'static [Option<OpInfo>],
        enc: Enc,
        op: u32,
    ) -> Result<&'static OpInfo, DecodeError> {
        table
            .get(op as usize)
            .and_then(|o| o.as_ref())
            .ok_or_else(|| self.err(ErrorKind::UnknownOpcode { enc, opcode: op }))
    }

    /// Builds the instruction from the table row and the decoded fields. `base` is the number
    /// of dwords before any literal / extension; `ext` is the DPP / SDWA dword if present.
    fn finish(
        &self,
        enc: Enc,
        opcode: u32,
        info: &'static OpInfo,
        f: &Fields,
        base: usize,
        ext: Option<u32>,
    ) -> Result<Instr, DecodeError> {
        let uses_literal = info.slots.iter().any(|s| match s {
            Slot::Lit => true,
            Slot::SSrc0(_) => f.ssrc0 == 255,
            Slot::SSrc1(_) => f.ssrc1 == 255,
            Slot::Src0(_) => f.src0 == 255 && ext.is_none(),
            Slot::Src1(_) => f.src1 == 255,
            Slot::Src2(_) => f.src2 == 255,
            _ => false,
        });
        let mut words = [0u32; 3];
        let mut n = base;
        for (i, w) in words.iter_mut().enumerate().take(base) {
            *w = self.dword(i)?;
        }
        if let Some(e) = ext {
            words[n] = e;
            n += 1;
        }
        let mut lit = 0;
        if uses_literal {
            lit = self.dword(n)?;
            words[n] = lit;
            n += 1;
        }
        let mask = if self.wave32 { 1 } else { 2 };
        let w = |w: &u8| if *w == 0 { mask } else { *w };
        let mut operands = Vec::with_capacity(info.slots.len());
        for s in info.slots {
            let sreg = |code: u32, count: u8| {
                if code == 255 {
                    Operand::Literal(lit)
                } else {
                    Operand::Reg {
                        code: code as u16,
                        count,
                    }
                }
            };
            operands.push(match s {
                Slot::SDst(c) => Operand::Reg {
                    code: f.sdst as u16,
                    count: w(c),
                },
                Slot::SSrc0(c) => sreg(f.ssrc0, w(c)),
                Slot::SSrc1(c) => sreg(f.ssrc1, w(c)),
                Slot::VDst(c) => Operand::Reg {
                    code: 256 + f.vdst as u16,
                    count: w(c),
                },
                Slot::VDstS(c) => Operand::Reg {
                    code: f.vdst as u16,
                    count: w(c),
                },
                Slot::Src0(c) => match ext {
                    Some(e) => Operand::Reg {
                        code: if enc == Enc::Vop1 || enc == Enc::Vop2 || enc == Enc::Vopc {
                            ext_src0(e, f.src0)
                        } else {
                            f.src0 as u16
                        },
                        count: w(c),
                    },
                    None => sreg(f.src0, w(c)),
                },
                Slot::VSrc1(c) => Operand::Reg {
                    // SDWA can name a scalar register or constant here (`s1` bit)
                    code: if f.src0 == 249 && ext.is_some_and(|e| e >> 31 != 0) {
                        f.vsrc1 as u16
                    } else {
                        256 + f.vsrc1 as u16
                    },
                    count: w(c),
                },
                Slot::Src1(c) => sreg(f.src1, w(c)),
                Slot::Src2(c) => sreg(f.src2, w(c)),
                Slot::SDstB(c) => Operand::Reg {
                    code: f.sdstb as u16,
                    count: w(c),
                },
                Slot::SBase(c) => Operand::Reg {
                    code: (f.sbase * 2) as u16,
                    count: *c,
                },
                Slot::SData(c) => Operand::Reg {
                    code: f.sdata as u16,
                    count: *c,
                },
                Slot::SDataImm => Operand::Imm(f.sdata),
                Slot::Attr => Operand::Imm(f.src0),
                Slot::Vdata(c) => Operand::Reg {
                    code: 256 + f.vdata as u16,
                    count: *c + f.extra,
                },
                Slot::Lit => Operand::Literal(lit),
                Slot::Fixed(t) => Operand::Fixed(t),
                Slot::Waitcnt
                | Slot::Sendmsg
                | Slot::Endpgm
                | Slot::Hwreg
                | Slot::Version
                | Slot::Dec16
                | Slot::Hex16
                | Slot::Imm16 => Operand::Imm(f.imm16),
            });
        }
        Ok(Instr {
            at: self.at,
            len: 4 * n,
            enc,
            opcode: opcode as u16,
            info,
            operands,
            words,
            nwords: n as u8,
            mods: Mods::default(),
            dpp: None,
            sdwa: None,
            mem: Mem::default(),
            exp: Exp::default(),
            intrp: Intrp::default(),
            wave32: self.wave32,
        })
    }

    fn sop1(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 8) & 0xff;
        let info = self.lookup(&table::SOP1, Enc::Sop1, op)?;
        let f = Fields {
            sdst: (d0 >> 16) & 0x7f,
            ssrc0: d0 & 0xff,
            ..Fields::default()
        };
        self.finish(Enc::Sop1, op, info, &f, 1, None)
    }

    fn sop2(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 23) & 0x7f;
        let info = self.lookup(&table::SOP2, Enc::Sop2, op)?;
        let f = Fields {
            sdst: (d0 >> 16) & 0x7f,
            ssrc0: d0 & 0xff,
            ssrc1: (d0 >> 8) & 0xff,
            ..Fields::default()
        };
        self.finish(Enc::Sop2, op, info, &f, 1, None)
    }

    fn sopc(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 16) & 0x7f;
        let info = self.lookup(&table::SOPC, Enc::Sopc, op)?;
        let f = Fields {
            ssrc0: d0 & 0xff,
            ssrc1: (d0 >> 8) & 0xff,
            ..Fields::default()
        };
        self.finish(Enc::Sopc, op, info, &f, 1, None)
    }

    fn sopk(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 23) & 0x1f;
        let info = self.lookup(&table::SOPK, Enc::Sopk, op)?;
        let f = Fields {
            sdst: (d0 >> 16) & 0x7f,
            imm16: d0 & 0xffff,
            ..Fields::default()
        };
        self.finish(Enc::Sopk, op, info, &f, 1, None)
    }

    fn sopp(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 16) & 0x7f;
        let info = self.lookup(&table::SOPP, Enc::Sopp, op)?;
        let f = Fields {
            imm16: d0 & 0xffff,
            ..Fields::default()
        };
        self.finish(Enc::Sopp, op, info, &f, 1, None)
    }

    fn smem(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 >> 18) & 0xff;
        let info = self.lookup(&table::SMEM, Enc::Smem, op)?;
        let f = Fields {
            sbase: d0 & 0x3f,
            sdata: (d0 >> 6) & 0x7f,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Smem, op, info, &f, 2, None)?;
        // 21-bit signed byte offset
        let off = ((d1 & 0x1f_ffff) << 11) as i32 >> 11;
        i.mem = Mem {
            offset: off,
            soffset: (d1 >> 25) as u16,
            glc: d0 >> 16 & 1 != 0,
            dlc: d0 >> 14 & 1 != 0,
            ..Mem::default()
        };
        Ok(i)
    }

    fn mubuf(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 >> 18) & 0xff;
        let info = self.lookup(&table::MUBUF, Enc::Mubuf, op)?;
        let tfe = d1 >> 23 & 1 != 0;
        let f = Fields {
            vdata: (d1 >> 8) & 0xff,
            extra: u8::from(tfe),
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Mubuf, op, info, &f, 2, None)?;
        i.mem = Mem {
            offset: (d0 & 0xfff) as i32,
            soffset: (d1 >> 24) as u16,
            vaddr: d1 as u8,
            srsrc: (d1 >> 16 & 0x1f) as u8 * 4,
            glc: d0 >> 14 & 1 != 0,
            dlc: d0 >> 15 & 1 != 0,
            offen: d0 >> 12 & 1 != 0,
            idxen: d0 >> 13 & 1 != 0,
            lds: d0 >> 16 & 1 != 0,
            slc: d1 >> 22 & 1 != 0,
            tfe,
        };
        Ok(i)
    }

    fn exp(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let info = self.lookup(&table::EXP, Enc::Exp, 0)?;
        let mut i = self.finish(Enc::Exp, 0, info, &Fields::default(), 2, None)?;
        i.exp = Exp {
            en: (d0 & 0xf) as u8,
            target: (d0 >> 4 & 0x3f) as u8,
            compr: d0 >> 10 & 1 != 0,
            done: d0 >> 11 & 1 != 0,
            vm: d0 >> 12 & 1 != 0,
            vsrc: d1.to_le_bytes(),
        };
        Ok(i)
    }

    fn vintrp(&self, d0: u32) -> Result<Instr, DecodeError> {
        let op = (d0 >> 16) & 3;
        let info = self.lookup(&table::VINTRP, Enc::Vintrp, op)?;
        let mut i = self.finish(Enc::Vintrp, op, info, &Fields::default(), 1, None)?;
        i.intrp = Intrp {
            vdst: (d0 >> 18) as u8,
            vsrc: d0 as u8,
            attr: (d0 >> 10 & 0x3f) as u8,
            chan: (d0 >> 8 & 3) as u8,
        };
        Ok(i)
    }

    fn vop12c(&self, enc: Enc, d0: u32) -> Result<Instr, DecodeError> {
        let src0 = d0 & 0x1ff;
        let (op, table, f) = match enc {
            Enc::Vop1 => (
                (d0 >> 9) & 0xff,
                &table::VOP1[..],
                Fields {
                    vdst: (d0 >> 17) & 0xff,
                    src0,
                    ..Fields::default()
                },
            ),
            Enc::Vop2 => (
                (d0 >> 25) & 0x3f,
                &table::VOP2[..],
                Fields {
                    vdst: (d0 >> 17) & 0xff,
                    src0,
                    vsrc1: (d0 >> 9) & 0xff,
                    ..Fields::default()
                },
            ),
            _ => (
                (d0 >> 17) & 0xff,
                &table::VOPC[..],
                Fields {
                    src0,
                    vsrc1: (d0 >> 9) & 0xff,
                    ..Fields::default()
                },
            ),
        };
        let info = self.lookup(table, enc, op)?;
        let kind = match src0 {
            233 => Some(DppKind::Dpp8),
            234 => Some(DppKind::Dpp8Fi),
            250 => Some(DppKind::Dpp16),
            _ => None,
        };
        let sdwa = src0 == 249;
        let ext = if kind.is_some() || sdwa {
            Some(self.dword(1)?)
        } else {
            None
        };
        let mut i = self.finish(enc, op, info, &f, 1, ext)?;
        if let Some(e) = ext {
            if sdwa && enc == Enc::Vopc && e >> 15 & 1 != 0 {
                // the compare writes an SGPR mask instead of VCC
                let mask = if self.wave32 { 1 } else { 2 };
                i.operands[0] = Operand::Reg {
                    code: (e >> 8 & 0x7f) as u16,
                    count: mask,
                };
            }
            if sdwa {
                i.sdwa = Some(Sdwa {
                    dst_sel: (e >> 8 & 7) as u8,
                    dst_unused: (e >> 11 & 3) as u8,
                    clamp: enc != Enc::Vopc && e >> 13 & 1 != 0,
                    omod: if enc == Enc::Vopc {
                        0
                    } else {
                        (e >> 14 & 3) as u8
                    },
                    src0_sel: (e >> 16 & 7) as u8,
                    src0_sext: e >> 19 & 1 != 0,
                    src0_neg: e >> 20 & 1 != 0,
                    src0_abs: e >> 21 & 1 != 0,
                    s0: e >> 23 & 1 != 0,
                    src1_sel: (e >> 24 & 7) as u8,
                    src1_sext: e >> 27 & 1 != 0,
                    src1_neg: e >> 28 & 1 != 0,
                    src1_abs: e >> 29 & 1 != 0,
                    s1: e >> 31 & 1 != 0,
                    sdst: (e >> 8 & 0x7f) as u8,
                    sd: e >> 15 & 1 != 0,
                });
            } else if let Some(kind) = kind {
                let dpp16 = kind == DppKind::Dpp16;
                let mut sel = [0u8; 8];
                for (k, s) in sel.iter_mut().enumerate() {
                    *s = (e >> (8 + 3 * k) & 7) as u8;
                }
                i.dpp = Some(Dpp {
                    kind,
                    ctrl: (e >> 8 & 0x1ff) as u16,
                    bound_ctrl: e >> 19 & 1 != 0,
                    fi: kind == DppKind::Dpp8Fi || dpp16 && e >> 18 & 1 != 0,
                    src0_neg: dpp16 && e >> 20 & 1 != 0,
                    src0_abs: dpp16 && e >> 21 & 1 != 0,
                    src1_neg: dpp16 && e >> 22 & 1 != 0,
                    src1_abs: dpp16 && e >> 23 & 1 != 0,
                    bank_mask: (e >> 24 & 0xf) as u8,
                    row_mask: (e >> 28 & 0xf) as u8,
                    sel,
                });
            }
        }
        Ok(i)
    }

    fn vop3(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 >> 16) & 0x3ff;
        let info = self.lookup(&table::VOP3, Enc::Vop3, op)?;
        let has_sdst = info.slots.iter().any(|s| matches!(s, Slot::SDstB(_)));
        let f = Fields {
            vdst: d0 & 0xff,
            sdstb: (d0 >> 8) & 0x7f,
            src0: d1 & 0x1ff,
            src1: (d1 >> 9) & 0x1ff,
            src2: (d1 >> 18) & 0x1ff,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Vop3, op, info, &f, 2, None)?;
        i.mods = Mods {
            abs: if has_sdst { 0 } else { (d0 >> 8 & 7) as u8 },
            opsel: if has_sdst { 0 } else { (d0 >> 11 & 0xf) as u8 },
            clamp: d0 >> 15 & 1 != 0,
            omod: (d1 >> 27 & 3) as u8,
            neg: (d1 >> 29 & 7) as u8,
        };
        Ok(i)
    }
}

/// Source 0 of a DPP / SDWA instruction: the VGPR (or SGPR, SDWA `s0`) in the extension dword.
fn ext_src0(ext: u32, src0: u32) -> u16 {
    let n = (ext & 0xff) as u16;
    if src0 == 249 && ext >> 23 & 1 != 0 {
        n
    } else {
        256 + n
    }
}
