//! The shapes of the opcode tables (`table.rs`, generated): one `OpInfo` per opcode, naming the
//! operands in print order as the encoding fields that carry them.

/// One operand of an instruction, as the encoding field that carries it. The number is the
/// width in dwords; 0 means "one lane mask" (2 dwords in wave64, 1 in wave32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// SOP1/SOP2/SOPK destination (7-bit scalar code).
    SDst(u8),
    /// SOP source 0 / 1 (8-bit scalar code, 255 = literal).
    SSrc0(u8),
    SSrc1(u8),
    /// VGPR destination (8-bit).
    VDst(u8),
    /// The same field read as a scalar register (v_readlane, VOP3 compares).
    VDstS(u8),
    /// VOP source 0 (9-bit code; VOP3 source 0 too) / VOP2 and VOPC VGPR source 1 / VOP3 sources.
    Src0(u8),
    VSrc1(u8),
    Src1(u8),
    Src2(u8),
    /// VOP3b scalar destination (bits 14:8 of the first dword).
    SDstB(u8),
    /// SMEM base (6-bit field times two) and data (7-bit scalar code) registers.
    SBase(u8),
    SData(u8),
    /// SMEM data field printed as a plain number (`s_atc_probe`).
    SDataImm,
    /// MUBUF data VGPRs.
    Vdata(u8),
    /// VOP3 interpolation attribute: source 0 field read as `attr.chan`.
    Attr,
    /// MIMG operands: data VGPRs, address VGPRs, resource and sampler SGPRs (the registers
    /// come from `Instr::img`).
    ImgData,
    ImgAddr,
    ImgRsrc,
    ImgSamp,
    /// DS address (VGPR) and data VGPRs; the destination is `VDst`.
    DsAddr,
    DsData0(u8),
    DsData1(u8),
    /// The extra literal dword.
    Lit,
    /// An implicit operand printed as written (`vcc` prints `vcc_lo` in wave32).
    Fixed(&'static str),
    /// 16-bit immediates and how LLVM prints them.
    Waitcnt,
    Sendmsg,
    Endpgm,
    Hwreg,
    Version,
    /// Decimal.
    Dec16,
    /// Hexadecimal.
    Hex16,
    /// Decimal up to 64, hexadecimal above.
    Imm16,
}

/// A table row.
#[derive(Clone, Copy, Debug)]
pub struct OpInfo {
    pub name: &'static str,
    pub slots: &'static [Slot],
    /// `FLAG_*` bits.
    pub flags: u32,
}

/// LLVM prints `_e32` / `_e64` after the name when the instruction exists in both forms.
pub const FLAG_SUFFIX: u32 = 1;
/// Only gfx1030 has this opcode (gfx1013, the title's target, does not).
pub const FLAG_G30: u32 = 2;
/// SMEM: LLVM prints the `glc` / `dlc` bits of this opcode.
pub const FLAG_GLC: u32 = 4;
pub const FLAG_DLC: u32 = 8;
/// A literal operand prints as 16 bits (f16 / i16 ops).
pub const FLAG_LIT16: u32 = 16;
/// VOP3: LLVM prints `op_sel` for this opcode, with `(flags >> 12) & 15` entries.
pub const FLAG_OPSEL: u32 = 32;
/// VOP3: bits 16..=18 say which sources take the abs / neg modifiers.
pub const FLAG_FMODS_SHIFT: u32 = 16;
/// VOP3: `clamp` and the output modifier apply.
pub const FLAG_CLAMP: u32 = 256;
pub const FLAG_OMOD: u32 = 512;

/// MIMG opcode properties (bits shared with the VOP3-only flags above, which MIMG never uses).
pub const FLAG_IMG_SAMP: u32 = 1 << 6;
pub const FLAG_IMG_GATHER: u32 = 1 << 7;
pub const FLAG_IMG_ATOMIC: u32 = 1 << 10;
pub const FLAG_IMG_CMPSWAP: u32 = 1 << 11;
/// Address pieces besides the coordinates: offset, bias, compare, derivatives, lod, clamp, mip.
pub const FLAG_IMG_OFFSET: u32 = 1 << 12;
pub const FLAG_IMG_BIAS: u32 = 1 << 13;
pub const FLAG_IMG_COMPARE: u32 = 1 << 14;
pub const FLAG_IMG_DERIV: u32 = 1 << 15;
pub const FLAG_IMG_LOD: u32 = 1 << 19;
pub const FLAG_IMG_CLAMP: u32 = 1 << 20;
pub const FLAG_IMG_MIP: u32 = 1 << 21;
/// The derivatives are 16-bit (`_g16` opcodes).
pub const FLAG_IMG_G16: u32 = 1 << 22;
/// No coordinates (`image_get_resinfo`: the mip level only).
pub const FLAG_IMG_NOCOORD: u32 = 1 << 23;

/// DS: the offset prints as `offset0:` / `offset1:` (read2, write2, ...), or as a swizzle macro.
pub const FLAG_DS_OFFSET2: u32 = 1 << 6;
pub const FLAG_DS_SWIZZLE: u32 = 1 << 7;

/// `image_bvh_intersect_ray` / `image_bvh64_intersect_ray`: 4 data registers, a 4-dword
/// resource, 11 (12 for the 64-bit node pointer) address registers in the NSA form.
pub const FLAG_IMG_BVH: u32 = 1 << 24;
pub const FLAG_IMG_BVH64: u32 = 1 << 25;

/// Fills a table from `(opcode, row)` pairs.
pub const fn build<const N: usize>(rows: &[(u16, OpInfo)]) -> [Option<OpInfo>; N] {
    let mut t = [None; N];
    let mut i = 0;
    while i < rows.len() {
        t[rows[i].0 as usize] = Some(rows[i].1);
        i += 1;
    }
    t
}
