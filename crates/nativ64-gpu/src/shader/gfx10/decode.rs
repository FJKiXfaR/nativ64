//! `decode`: one instruction from machine code.

use super::ops::{self, OpInfo, Slot};
use super::{table, table_ext};
use super::{
    DecodeError, Dpp, DppKind, Enc, ErrorKind, Exp, Img, Instr, Intrp, Mem, Mods, Operand, Sdwa,
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
            0x3c => Enc::Mimg,
            0x3a => Enc::Mtbuf,
            0x36 => Enc::Ds,
            0x33 => Enc::Vop3p,
            0x37 => Enc::Flat,
            0x3d => Enc::Smem,
            0x3e => Enc::Exp,
            _ => {
                return Err(c.err(ErrorKind::UnknownEncoding {
                    name: "?",
                    word: d0,
                }));
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
        Enc::Mimg => c.mimg(d0),
        Enc::Mtbuf => c.mtbuf(d0),
        Enc::Ds => c.ds(d0),
        Enc::Vop3p => c.vop3p(d0),
        Enc::Flat => c.flat(d0),
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
    dsaddr: u32,
    data0: u32,
    data1: u32,
    vaddr: u32,
    saddr: u32,
    seg: u8,
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
                Slot::DsAddr => Operand::Reg {
                    code: 256 + f.dsaddr as u16,
                    count: 1,
                },
                Slot::DsData0(c) => Operand::Reg {
                    code: 256 + f.data0 as u16,
                    count: *c,
                },
                Slot::DsData1(c) => Operand::Reg {
                    code: 256 + f.data1 as u16,
                    count: *c,
                },
                Slot::FlatAddr => match (f.seg, f.saddr == 0x7d) {
                    (0, _) | (2, true) => Operand::Reg {
                        code: 256 + f.vaddr as u16,
                        count: 2,
                    },
                    (1, false) => Operand::Fixed("off"),
                    _ => Operand::Reg {
                        code: 256 + f.vaddr as u16,
                        count: 1,
                    },
                },
                Slot::FlatSaddr if f.saddr == 0x7d => Operand::Fixed("off"),
                Slot::FlatSaddr => Operand::Reg {
                    code: f.saddr as u16,
                    count: if f.seg == 2 { 2 } else { 1 },
                },
                Slot::ImgData | Slot::ImgAddr | Slot::ImgRsrc | Slot::ImgSamp => Operand::Imm(0),
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
            img: Img::default(),
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
            ..Mem::default()
        };
        Ok(i)
    }

    fn ds(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 >> 18) & 0xff;
        let info = self.lookup(&table_ext::DS, Enc::Ds, op)?;
        let f = Fields {
            dsaddr: d1 & 0xff,
            data0: (d1 >> 8) & 0xff,
            data1: (d1 >> 16) & 0xff,
            vdst: d1 >> 24,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Ds, op, info, &f, 2, None)?;
        i.mem = Mem {
            offset: (d0 & 0xffff) as i32,
            offset1: (d0 >> 8) as u8,
            gds: d0 >> 17 & 1 != 0,
            ..Mem::default()
        };
        Ok(i)
    }

    fn vop3p(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 >> 16) & 0x7f;
        let info = self.lookup(&table_ext::VOP3P, Enc::Vop3p, op)?;
        let nsrc = info
            .slots
            .iter()
            .filter(|s| matches!(s, Slot::Src0(_) | Slot::Src1(_) | Slot::Src2(_)))
            .count();
        let (neg_hi, opsel, neg_lo) = ((d0 >> 8 & 7) as u8, (d0 >> 11 & 7) as u8, (d1 >> 29) as u8);
        if (neg_hi | opsel | neg_lo) >> nsrc != 0 {
            return Err(self.err(ErrorKind::BadField("VOP3P modifier of a missing source")));
        }
        let f = Fields {
            vdst: d0 & 0xff,
            src0: d1 & 0x1ff,
            src1: (d1 >> 9) & 0x1ff,
            src2: (d1 >> 18) & 0x1ff,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Vop3p, op, info, &f, 2, None)?;
        i.mods = Mods {
            abs: neg_hi,
            neg: neg_lo,
            opsel,
            clamp: d0 >> 15 & 1 != 0,
            omod: 0,
            opsel_hi: (d1 >> 27 & 3) as u8 | ((d0 >> 14 & 1) as u8) << 2,
        };
        Ok(i)
    }

    fn flat(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let seg = (d0 >> 14 & 3) as u8;
        let (table, enc_seg) = match seg {
            0 => (&table_ext::FLAT, 0),
            1 => (&table_ext::SCRATCH, 1),
            2 => (&table_ext::GLOBAL, 2),
            _ => return Err(self.err(ErrorKind::BadField("FLAT segment"))),
        };
        let op = (d0 >> 18) & 0x7f;
        let info = self.lookup(table, Enc::Flat, op)?;
        let saddr = d1 >> 16 & 0x7f;
        // the scalar base: absent (0x7d) for flat; a pair (not m0 / exec) for global
        let saddr_ok = match seg {
            0 => saddr == 0x7d,
            1 => saddr <= 0x7e,
            _ => saddr <= 106 || (108..=123).contains(&saddr) || saddr == 0x7d,
        };
        if !saddr_ok {
            return Err(self.err(ErrorKind::BadField("FLAT saddr")));
        }
        if d1 >> 23 & 1 != 0 {
            return Err(self.err(ErrorKind::BadField("FLAT nv")));
        }
        let lds = d0 >> 13 & 1 != 0;
        if lds && info.flags & ops::FLAG_FLAT_LDS == 0 {
            return Err(self.err(ErrorKind::BadField("FLAT lds")));
        }
        let f = Fields {
            vaddr: d1 & 0xff,
            vdata: d1 >> 8 & 0xff,
            saddr,
            vdst: d1 >> 24,
            seg: enc_seg,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Flat, op, info, &f, 2, None)?;
        // 12-bit offset: signed except for the flat segment
        let off = (d0 & 0xfff) as i32;
        i.mem = Mem {
            offset: if seg == 0 { off } else { (off << 20) >> 20 },
            glc: d0 >> 16 & 1 != 0,
            slc: d0 >> 17 & 1 != 0,
            dlc: d0 >> 12 & 1 != 0,
            lds,
            seg,
            ..Mem::default()
        };
        Ok(i)
    }

    fn mtbuf(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        // the d16 forms are opcodes of their own
        let op = (d0 >> 16 & 7) | (d1 >> 21 & 1) << 3;
        let info = self.lookup(&table_ext::MTBUF, Enc::Mtbuf, op)?;
        if d1 >> 23 & 1 != 0 {
            // LLVM has no tfe form of the typed buffer instructions
            return Err(self.err(ErrorKind::BadField("MTBUF tfe")));
        }
        let f = Fields {
            vdata: (d1 >> 8) & 0xff,
            ..Fields::default()
        };
        let mut i = self.finish(Enc::Mtbuf, op, info, &f, 2, None)?;
        i.mem = Mem {
            offset: (d0 & 0xfff) as i32,
            soffset: (d1 >> 24) as u16,
            vaddr: d1 as u8,
            srsrc: (d1 >> 16 & 0x1f) as u8 * 4,
            glc: d0 >> 14 & 1 != 0,
            dlc: d0 >> 15 & 1 != 0,
            offen: d0 >> 12 & 1 != 0,
            idxen: d0 >> 13 & 1 != 0,
            slc: d1 >> 22 & 1 != 0,
            format: (d0 >> 19 & 0x7f) as u8,
            ..Mem::default()
        };
        Ok(i)
    }

    fn mimg(&self, d0: u32) -> Result<Instr, DecodeError> {
        let d1 = self.dword(1)?;
        let op = (d0 & 1) << 7 | (d0 >> 18) & 0x7f;
        let info = self.lookup(&table_ext::MIMG, Enc::Mimg, op)?;
        let nsa = (d0 >> 1 & 3) as usize;
        let dim = (d0 >> 3 & 7) as u8;
        let a16 = d1 >> 30 & 1 != 0;
        let dmask = (d0 >> 8 & 0xf) as u8;
        let tfe = d0 >> 16 & 1 != 0;
        let d16 = d1 >> 31 & 1 != 0;
        let bvh = info.flags & ops::FLAG_IMG_BVH != 0;
        if bvh && a16 {
            return Err(self.err(ErrorKind::BadField("BVH a16")));
        }
        let naddr = if bvh {
            11 + u8::from(info.flags & ops::FLAG_IMG_BVH64 != 0)
        } else {
            img_naddr(info.flags, dim, a16)
        };
        // the data registers: one per channel of the mask (an atomic's valid masks already
        // count its operands: 1 or 3, and 3 or 15 for compare-and-swap), halved by d16
        let f = info.flags;
        let mut ndata = if f & ops::FLAG_IMG_GATHER != 0 || bvh {
            4
        } else {
            dmask.count_ones() as u8
        };
        if d16 && f & ops::FLAG_IMG_ATOMIC == 0 {
            ndata = ndata.div_ceil(2);
        }
        ndata += u8::from(tfe);
        let mut img = Img {
            dmask,
            dim,
            nsa: nsa as u8,
            naddr,
            vdata: (d1 >> 8) as u8,
            ndata: ndata.max(1),
            srsrc: ((d1 >> 16 & 0x1f) * 4) as u8,
            ssamp: ((d1 >> 21 & 0x1f) * 4) as u8,
            unorm: d0 >> 12 & 1 != 0,
            glc: d0 >> 13 & 1 != 0,
            slc: d0 >> 25 & 1 != 0,
            dlc: d0 >> 7 & 1 != 0,
            r128: d0 >> 15 & 1 != 0,
            a16,
            tfe,
            lwe: d0 >> 17 & 1 != 0,
            d16,
            ..Img::default()
        };
        img.vaddr[0] = d1 as u8;
        if nsa > 0 {
            // one byte per further address register, four to a dword
            if usize::from(naddr) > 1 + 4 * nsa {
                return Err(self.err(ErrorKind::BadField("MIMG address count")));
            }
            for (k, slot) in img
                .vaddr
                .iter_mut()
                .enumerate()
                .take(usize::from(naddr))
                .skip(1)
            {
                let w = self.dword(2 + (k - 1) / 4)?;
                *slot = (w >> (8 * ((k - 1) % 4))) as u8;
            }
        } else {
            for k in 1..usize::from(naddr) {
                img.vaddr[k] = img.vaddr[0].wrapping_add(k as u8);
            }
        }
        let mut i = self.finish(Enc::Mimg, op, info, &Fields::default(), 2 + nsa, None)?;
        let vcount = if nsa > 0 { 1 } else { naddr.max(1) };
        for (s, o) in info.slots.iter().zip(i.operands.iter_mut()) {
            *o = match s {
                Slot::ImgData => Operand::Reg {
                    code: 256 + u16::from(img.vdata),
                    count: img.ndata,
                },
                Slot::ImgAddr => Operand::Reg {
                    code: 256 + u16::from(img.vaddr[0]),
                    count: vcount,
                },
                Slot::ImgRsrc => Operand::Reg {
                    code: u16::from(img.srsrc),
                    count: if bvh { 4 } else { 8 },
                },
                _ => Operand::Reg {
                    code: u16::from(img.ssamp),
                    count: 4,
                },
            };
        }
        i.img = img;
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
            opsel_hi: 0,
        };
        Ok(i)
    }
}

/// The address dwords of an image instruction: offset, bias, compare, derivatives, the
/// coordinates and lod / clamp / mip; with `a16` the coordinates and those last three pack in
/// halves (the derivatives only pack in the `_g16` opcodes).
fn img_naddr(flags: u32, dim: u8, a16: bool) -> u8 {
    const COORDS: [u8; 8] = [1, 2, 3, 3, 2, 3, 3, 4];
    const GRADS: [u8; 8] = [1, 2, 3, 2, 1, 2, 2, 2];
    let has = |f: u32| u8::from(flags & f != 0);
    if flags & ops::FLAG_IMG_NOCOORD != 0 {
        return 1;
    }
    let d = dim as usize & 7;
    let mut n = has(ops::FLAG_IMG_OFFSET) + has(ops::FLAG_IMG_BIAS) + has(ops::FLAG_IMG_COMPARE);
    if flags & ops::FLAG_IMG_DERIV != 0 {
        let g = GRADS[d];
        n += if flags & ops::FLAG_IMG_G16 != 0 {
            2 * g.div_ceil(2)
        } else {
            2 * g
        };
    }
    let packed =
        COORDS[d] + has(ops::FLAG_IMG_LOD) + has(ops::FLAG_IMG_CLAMP) + has(ops::FLAG_IMG_MIP);
    n + if a16 { packed.div_ceil(2) } else { packed }
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
