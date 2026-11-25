//! The address equation of a tiled swizzle mode: where a texel lands inside its block.
//!
//! Every bit of the offset is the XOR of a few bits of x, y and z (the sample bits stay zero,
//! MSAA is refused), so the whole equation splits into one table per axis:
//! `offset = base_xor ^ ox[x] ^ oy[y] ^ oz[z]`, with x, y and z the texel coordinates inside
//! the block, in elements. The patterns come from addrlib (`swizzle_tables.rs`), written down
//! by AMD for each mode, element size and pipe configuration.

use super::swizzle_tables as t;

/// A swizzle mode, by addrlib's number (0 linear, 1/2 256B_S/D, 5/6 4KB_S/D, 9/10 64KB_S/D,
/// 17/18 64KB_S_T/D_T, 21-23 4KB_S_X/D_X/R_X, 24-27 64KB_Z_X/S_X/D_X/R_X, 28-31 VAR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwizzleMode(pub u32);

impl SwizzleMode {
    pub const LINEAR: Self = Self(0);
    pub const SW_4KB_S: Self = Self(5);
    pub const SW_4KB_S_X: Self = Self(21);
    pub const SW_64KB_Z_X: Self = Self(24);
    pub const SW_64KB_R_X: Self = Self(27);

    pub fn is_linear(self) -> bool {
        self.0 == 0
    }

    /// The variable-size modes (28-31).
    pub fn is_var(self) -> bool {
        (28..=31).contains(&self.0)
    }

    /// Modes we have patterns for (linear is a mode, but has no equation). 4KB_R_X has no
    /// pattern in addrlib either.
    pub fn is_supported(self) -> bool {
        matches!(
            self.0,
            0 | 1 | 2 | 5 | 6 | 9 | 10 | 17 | 18 | 21 | 22 | 24..=27
        )
    }

    /// log2 of the block size in bytes: 8, 12 or 16 (linear counts as 256 B).
    pub fn block_log2(self) -> u32 {
        match self.0 {
            0..=2 => 8,
            5 | 6 | 21..=23 => 12,
            _ => 16,
        }
    }

    pub fn block_bytes(self) -> u64 {
        1 << self.block_log2()
    }

    /// Modes whose offset takes the surface's pipe/bank XOR (`_T` and `_X`).
    pub fn is_xor(self) -> bool {
        matches!(self.0, 17 | 18 | 21..=27)
    }

    pub fn is_z(self) -> bool {
        self.0 == 24
    }

    pub fn is_render(self) -> bool {
        matches!(self.0, 23 | 27)
    }

    /// Standard (`_S`) or display (`_D`) modes: the ones with thick blocks in 3D.
    pub fn is_std_or_disp(self) -> bool {
        matches!(self.0, 1 | 2 | 5 | 6 | 9 | 10 | 17 | 18 | 21 | 22 | 25 | 26)
    }

    /// Modes a 3D texture may use (addrlib's `Gfx10Rsrc3dSwModeMask`).
    pub fn allowed_3d(self) -> bool {
        matches!(self.0, 0 | 5 | 9 | 17 | 21 | 24..=27)
    }

    /// Modes a 1D texture may use (linear, Z and R; the VAR ones are refused earlier).
    pub fn allowed_1d(self) -> bool {
        matches!(self.0, 0 | 24 | 27)
    }
}

/// The console's address configuration: the parts that the patterns depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub pipes_log2: u32,
    pub pkrs_log2: u32,
}

/// Series X: 32 pipes, 8 packers.
pub const SERIES_X: Config = Config {
    pipes_log2: 5,
    pkrs_log2: 3,
};
/// Series S: 8 pipes, 4 packers.
pub const SERIES_S: Config = Config {
    pipes_log2: 3,
    pkrs_log2: 2,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EquationError {
    /// Linear, VAR, 4KB_R_X, or a number that is no mode: no pattern.
    NoPattern(u32),
    /// A configuration other than the two consoles.
    UnknownConfig,
    /// Elements of other than 1, 2, 4, 8 or 16 bytes.
    ElementSize(u32),
}

/// The pattern table of a mode, for a 2D or a 3D texture (thick blocks have their own).
fn rows(mode: SwizzleMode, thick: bool) -> Option<&'static t::Rows> {
    Some(match (mode.0, thick) {
        (1, _) => &t::SW_256B_S,
        (2, _) => &t::SW_256B_D,
        (5, false) => &t::SW_4KB_S,
        (5, true) => &t::SW_4KB_S_3D,
        (6, _) => &t::SW_4KB_D,
        (9, false) => &t::SW_64KB_S,
        (9, true) => &t::SW_64KB_S_3D,
        (10, _) => &t::SW_64KB_D,
        (17, false) => &t::SW_64KB_S_T,
        (17, true) => &t::SW_64KB_S_T_3D,
        (18, _) => &t::SW_64KB_D_T,
        (21, false) => &t::SW_4KB_S_X,
        (21, true) => &t::SW_4KB_S_X_3D,
        (22, _) => &t::SW_4KB_D_X,
        (24, _) => &t::SW_64KB_Z_X,
        (25, false) => &t::SW_64KB_S_X,
        (25, true) => &t::SW_64KB_S_X_3D,
        (26, false) => &t::SW_64KB_D_X,
        (26, true) => &t::SW_64KB_D_X_3D,
        (27, _) => &t::SW_64KB_R_X,
        _ => return None,
    })
}

/// One axis of the equation: `table[c & (len - 1)]` is what coordinate `c` adds.
fn axis(masks: &[u16], len_bits: u32) -> Vec<u32> {
    (0..1u32 << len_bits)
        .map(|c| {
            masks.iter().enumerate().fold(0, |acc, (bit, &m)| {
                acc | (((c & m as u32).count_ones() & 1) << bit)
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equation {
    /// Offset bits: the block size, log2.
    pub block_log2: u32,
    pub ox: Vec<u32>,
    pub oy: Vec<u32>,
    pub oz: Vec<u32>,
}

impl Equation {
    /// The equation of `mode` for elements of `1 << elem_log2` bytes. `thick` is a 3D texture
    /// with thick blocks (only meaningful for the modes that have them).
    pub fn new(
        mode: SwizzleMode,
        thick: bool,
        elem_log2: u32,
        config: Config,
    ) -> Result<Equation, EquationError> {
        if elem_log2 > 4 {
            return Err(EquationError::ElementSize(1 << elem_log2));
        }
        let rows = rows(mode, thick).ok_or(EquationError::NoPattern(mode.0))?;
        let group = if !mode.is_xor() {
            0
        } else if config == SERIES_X {
            1
        } else if config == SERIES_S {
            2
        } else {
            return Err(EquationError::UnknownConfig);
        };
        let p = rows[group][elem_log2 as usize];
        let block_log2 = mode.block_log2();
        let (mut xm, mut ym, mut zm) = (Vec::new(), Vec::new(), Vec::new());
        for bit in 0..block_log2 as usize {
            let term = match bit {
                0..=7 => t::NIBBLE01[p.n01 as usize][bit],
                8..=11 => t::NIBBLE2[p.n2 as usize][bit - 8],
                12..=15 => t::NIBBLE3[p.n3 as usize][bit - 12],
                _ => t::NIBBLE4[p.n4 as usize][bit - 16],
            };
            debug_assert_eq!(term >> 48, 0, "sample bits in a single-sample pattern");
            xm.push(term as u16);
            ym.push((term >> 16) as u16);
            zm.push((term >> 32) as u16);
        }
        let width = |masks: &[u16]| {
            let all = masks.iter().fold(0u16, |a, &m| a | m);
            16 - all.leading_zeros()
        };
        Ok(Equation {
            block_log2,
            ox: axis(&xm, width(&xm)),
            oy: axis(&ym, width(&ym)),
            oz: axis(&zm, width(&zm)),
        })
    }

    /// Offset inside the block of the texel at (`x`, `y`, `z`) in elements, inside the block.
    /// The byte inside the element is added by the caller (the low bits are zero here).
    pub fn offset(&self, base_xor: u32, x: u32, y: u32, z: u32) -> u32 {
        let at = |t: &[u32], c: u32| t[(c as usize) & (t.len() - 1)];
        base_xor ^ at(&self.ox, x) ^ at(&self.oy, y) ^ at(&self.oz, z)
    }
}

/// The value XORed into the offsets of a surface placed at `base` (what addrlib calls the
/// pipe/bank XOR): the pipe and bank bits of the base address, inside the block. Zero for
/// the modes that do not XOR.
pub fn pipe_bank_xor(base: u64, mode: SwizzleMode, config: Config) -> u32 {
    const PIPE_INTERLEAVE_LOG2: u32 = 8;
    const COLUMN_BITS: u32 = 2;
    const BANK_BITS: u32 = 4;
    if !mode.is_xor() {
        return 0;
    }
    let blk = mode.block_log2();
    let pipe_mask = (1u32 << config.pipes_log2) - 1;
    let bank_bits = blk
        .saturating_sub(PIPE_INTERLEAVE_LOG2 + config.pipes_log2 + COLUMN_BITS)
        .min(BANK_BITS);
    let bank_mask = ((1u32 << bank_bits) - 1) << (config.pipes_log2 + COLUMN_BITS);
    let value = (base >> PIPE_INTERLEAVE_LOG2) as u32 & (pipe_mask | bank_mask);
    (value << PIPE_INTERLEAVE_LOG2) & ((1u32 << blk) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture::layout::block_dims;

    const MODES: [u32; 14] = [1, 2, 5, 6, 9, 10, 17, 18, 21, 22, 24, 25, 26, 27];

    #[test]
    fn hand_checked_256b_standard() {
        // 256B_S, 4-byte elements: offset bits are -,-,X0,X1,Y0,Y1,Y2,X2 (addrlib pattern row)
        let e = Equation::new(SwizzleMode(1), false, 2, SERIES_X).unwrap();
        assert_eq!(e.block_log2, 8);
        assert_eq!(e.offset(0, 1, 0, 0), 4);
        assert_eq!(e.offset(0, 2, 0, 0), 8);
        assert_eq!(e.offset(0, 0, 1, 0), 16);
        assert_eq!(e.offset(0, 0, 4, 0), 64);
        assert_eq!(e.offset(0, 4, 0, 0), 128);
        assert_eq!(e.offset(0, 7, 7, 0), 252);
    }

    #[test]
    fn hand_checked_256b_display() {
        // 256B_D, 1-byte elements: offset bits X0,X1,X2,Y1,Y0,Y2,X3,Y3
        let e = Equation::new(SwizzleMode(2), false, 0, SERIES_X).unwrap();
        assert_eq!(e.offset(0, 1, 0, 0), 1);
        assert_eq!(e.offset(0, 4, 0, 0), 4);
        assert_eq!(e.offset(0, 0, 1, 0), 16);
        assert_eq!(e.offset(0, 0, 2, 0), 8);
        assert_eq!(e.offset(0, 0, 4, 0), 32);
        assert_eq!(e.offset(0, 8, 0, 0), 64);
        assert_eq!(e.offset(0, 0, 8, 0), 128);
        // the base XOR is applied as is
        assert_eq!(e.offset(0xa0, 0, 0, 0), 0xa0);
        assert_eq!(e.offset(0xa0, 1, 0, 0), 0xa1);
    }

    #[test]
    fn no_sample_bits_and_bounded() {
        for mode in MODES {
            for thick in [false, true] {
                for elem in 0..=4 {
                    for config in [SERIES_X, SERIES_S] {
                        let Ok(e) = Equation::new(SwizzleMode(mode), thick, elem, config) else {
                            continue;
                        };
                        let size = 1u32 << e.block_log2;
                        for t in [&e.ox, &e.oy, &e.oz] {
                            assert!(t.iter().all(|&v| v < size), "mode {mode}");
                            assert_eq!(t[0], 0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bijection_inside_a_block() {
        for mode in MODES {
            let m = SwizzleMode(mode);
            for thick in [false, true] {
                if thick && !(m.is_std_or_disp() && m.allowed_3d()) {
                    continue;
                }
                for elem in 0..=4u32 {
                    for config in [SERIES_X, SERIES_S] {
                        let Ok(e) = Equation::new(m, thick, elem, config) else {
                            continue;
                        };
                        let [w, h, d] = block_dims(m, thick, 1 << elem, 1);
                        assert_eq!(
                            (w * h * d) as u64 * (1 << elem),
                            m.block_bytes(),
                            "mode {mode} thick {thick} elem {elem}"
                        );
                        for base_xor in [0, pipe_bank_xor(0x1234_5678_9a00, m, config)] {
                            let mut seen = vec![false; (w * h * d) as usize];
                            for z in 0..d {
                                for y in 0..h {
                                    for x in 0..w {
                                        let off = e.offset(base_xor, x, y, z);
                                        assert_eq!(off & ((1 << elem) - 1), 0);
                                        let i = (off >> elem) as usize;
                                        assert!(
                                            i < seen.len() && !seen[i],
                                            "mode {mode} thick {thick} elem {elem}: {x},{y},{z}"
                                        );
                                        seen[i] = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn errors() {
        for mode in [0, 3, 23, 28, 31, 40] {
            assert_eq!(
                Equation::new(SwizzleMode(mode), false, 2, SERIES_X),
                Err(EquationError::NoPattern(mode))
            );
        }
        assert_eq!(
            Equation::new(SwizzleMode(27), false, 5, SERIES_X),
            Err(EquationError::ElementSize(32))
        );
        let other = Config {
            pipes_log2: 4,
            pkrs_log2: 2,
        };
        assert_eq!(
            Equation::new(SwizzleMode(27), false, 2, other),
            Err(EquationError::UnknownConfig)
        );
        // modes without XOR do not depend on the configuration
        assert!(Equation::new(SwizzleMode(9), false, 2, other).is_ok());
    }

    #[test]
    fn pipe_bank_xor_bits() {
        let base = 0x0123_4567_8900u64;
        assert_eq!(pipe_bank_xor(base, SwizzleMode(9), SERIES_X), 0);
        // 64KB_R_X on Series X: pipe bits 8..13 and one bank bit (15) of the base address
        assert_eq!(
            pipe_bank_xor(base, SwizzleMode(27), SERIES_X),
            (base & 0x9f00) as u32
        );
        // 4KB_S_X: no bank bits, pipe bits cut at the block size
        assert_eq!(
            pipe_bank_xor(base, SwizzleMode(21), SERIES_X),
            (base & 0x0f00) as u32
        );
        assert_eq!(
            pipe_bank_xor(base, SwizzleMode(27), SERIES_S),
            (base & 0xe700) as u32
        );
    }
}
