//! The root block of a draw (chunk 10a, `docs/shaders.md`, Chunk 10 design): what the console
//! would have put in the user SGPRs, built from the list's current root arguments and the
//! stage's layout rows, with every address translated to the device.
//!
//! ```text
//! u32 user_data[32]   at   0   a stage's `s[n]` in its user range is `user_data[n]`
//! u64 table_base[32]  at 128   per user SGPR the layout marks as a table: the full address
//! u64 view_table      at 384   device address of the view table
//! ```
//!
//! A row of the layout (`first dword` = SGPR n, `user sgprs` = count, `slot` = root parameter)
//! is filled by its root type: 0 constants (the values from `table offset` on), 1 root
//! descriptor (the translated address, two dwords), 3 table (the low 32 bits of the
//! translated handle in `user_data[n]`, the full address in `table_base[n]`), 5 and 6 the
//! resource and sampler heaps (their start, as a table: **[unverified]**, no blob with a heap
//! row has run), 4 static samplers (nothing). Rows of both stages are merged (one root
//! signature). A constant or root descriptor the title never set reads as 0; a table or heap
//! never set fails the draw (`FillError::Memory`: a shader would read at address 0).

use crate::d3d12::command::Command;
use crate::d3d12::descriptor::DescriptorHeapData;
use crate::shader::native::RootLayout;
use crate::xobj::XObject;
use std::collections::BTreeMap;
use std::ffi::c_void;

pub const USER_DWORDS: usize = 32;
const TABLE_BASE: usize = 4 * USER_DWORDS;
const VIEW_TABLE: usize = TABLE_BASE + 8 * USER_DWORDS;
/// Size of the block.
pub const BLOCK: usize = VIEW_TABLE + 8;
/// Distance between two blocks in the arena (the push constant is any address; this keeps
/// each block on its own cache lines).
pub const STRIDE: usize = 512;

/// The root arguments a list set on its graphics root signature, or (`RootArgs::compute`) on
/// its compute one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RootArgs {
    /// Folds the commands of the compute root signature in (chunk 10c).
    compute: bool,
    pub(super) constants: BTreeMap<u32, Vec<u32>>,
    pub(super) tables: BTreeMap<u32, u64>,
    pub(super) views: BTreeMap<u32, u64>,
    /// Start addresses of the bound resource and sampler heaps.
    resource_heap: u64,
    sampler_heap: u64,
}

impl RootArgs {
    /// The arguments of the compute root signature.
    pub fn compute() -> Self {
        RootArgs {
            compute: true,
            ..Default::default()
        }
    }

    /// Folds a state command in; false when `c` is none of ours.
    pub fn apply(&mut self, c: &Command) -> bool {
        match c {
            Command::RootSignature { compute, .. } if *compute == self.compute => {
                // a new root signature drops the arguments (the heaps stay bound)
                let (r, s) = (self.resource_heap, self.sampler_heap);
                *self = RootArgs {
                    compute: self.compute,
                    ..Default::default()
                };
                (self.resource_heap, self.sampler_heap) = (r, s);
            }
            Command::RootTable {
                compute,
                slot,
                handle,
            } if *compute == self.compute => {
                self.tables.insert(*slot, *handle);
            }
            Command::RootConstants {
                compute,
                slot,
                offset,
                values,
            } if *compute == self.compute => {
                let v = self.constants.entry(*slot).or_default();
                let end = *offset as usize + values.len();
                if v.len() < end {
                    v.resize(end, 0);
                }
                v[*offset as usize..end].copy_from_slice(values);
            }
            Command::RootView {
                compute,
                slot,
                address,
                ..
            } if *compute == self.compute => {
                self.views.insert(*slot, *address);
            }
            Command::Heaps(heaps) => {
                (self.resource_heap, self.sampler_heap) = (0, 0);
                for &h in heaps {
                    // SAFETY: a heap object the list recorded; its data outlives the list.
                    let d = unsafe { XObject::<DescriptorHeapData>::data(h as *mut c_void) };
                    match d.desc.kind {
                        0 => self.resource_heap = d.start,
                        1 => self.sampler_heap = d.start,
                        _ => {}
                    }
                }
            }
            Command::RootSignature { .. }
            | Command::RootTable { .. }
            | Command::RootConstants { .. }
            | Command::RootView { .. } => {}
            _ => return false,
        }
        true
    }
}

/// Why a root block cannot be built.
#[derive(Debug, PartialEq, Eq)]
pub enum FillError {
    /// An address is not GPU memory.
    Memory(String),
    /// The layout rows are inconsistent: they overflow the block or give one user SGPR two
    /// meanings (the stages of a pipeline share one block).
    Layout(String),
}

/// The root block for `rows`, or why it cannot be built. `translate` maps a title address to a
/// device address (None: it is not memory we can import).
pub fn fill(
    rows: &[RootLayout],
    args: &RootArgs,
    view_table: u64,
    translate: &mut dyn FnMut(u64) -> Option<u64>,
) -> Result<[u8; BLOCK], FillError> {
    let mut b = [0u8; BLOCK];
    // what each user dword was given: (root type, slot)
    let mut owner = [None; USER_DWORDS];
    let user = |b: &mut [u8; BLOCK], n: usize, v: u32| {
        b[4 * n..4 * n + 4].copy_from_slice(&v.to_le_bytes())
    };
    let mut tr = |a: u64, what: &str| -> Result<u64, FillError> {
        if a == 0 {
            return Ok(0);
        }
        translate(a).ok_or_else(|| FillError::Memory(format!("{what} at {a:#x} is not GPU memory")))
    };
    for r in rows {
        let (n, count) = (usize::from(r.first_user_dword), r.user_sgprs as usize);
        if n + count > USER_DWORDS {
            return Err(FillError::Layout(format!(
                "user SGPRs {n}..{} past the root block",
                n + count
            )));
        }
        let slot = u32::from(r.root_slot);
        if matches!(r.root_type, 0 | 1 | 3 | 5 | 6) {
            let mine = (r.root_type, slot);
            for (k, o) in owner.iter_mut().enumerate().skip(n).take(count.max(1)) {
                if o.is_some_and(|o| o != mine) {
                    return Err(FillError::Layout(format!(
                        "user SGPR {k} is root parameter {slot} (type {}) in one stage and \
                         something else in the other",
                        r.root_type
                    )));
                }
                *o = Some(mine);
            }
        }
        match r.root_type {
            0 => {
                let v = args.constants.get(&slot);
                for k in 0..count {
                    let x = v
                        .and_then(|v| v.get(r.table_offset as usize + k))
                        .copied()
                        .unwrap_or(0);
                    user(&mut b, n + k, x);
                }
            }
            1 => {
                let a = tr(
                    args.views.get(&slot).copied().unwrap_or(0),
                    "a root descriptor",
                )?;
                user(&mut b, n, a as u32);
                if count > 1 {
                    user(&mut b, n + 1, (a >> 32) as u32);
                }
            }
            3 | 5 | 6 => {
                let (from, what) = match r.root_type {
                    3 => (
                        args.tables.get(&slot).copied().unwrap_or(0),
                        "a descriptor table",
                    ),
                    5 => (args.resource_heap, "the resource heap"),
                    _ => (args.sampler_heap, "the sampler heap"),
                };
                // a shader reads through the table: an unset one would be a read at address 0
                if from == 0 {
                    return Err(FillError::Memory(format!("{what} is not set")));
                }
                let a = tr(from, what)?;
                user(&mut b, n, a as u32);
                b[TABLE_BASE + 8 * n..TABLE_BASE + 8 * n + 8].copy_from_slice(&a.to_le_bytes());
            }
            _ => {}
        }
    }
    b[VIEW_TABLE..VIEW_TABLE + 8].copy_from_slice(&view_table.to_le_bytes());
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::command::RootView;

    fn row(root_type: u32, table_offset: u32, count: u32, first: u8, slot: u8) -> RootLayout {
        RootLayout {
            root_type,
            table_offset,
            user_sgprs: count,
            base_register: 0,
            first_user_dword: first,
            root_slot: slot,
        }
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn constants_descriptors_and_tables_fill_their_dwords() {
        let mut args = RootArgs::default();
        let cmds = [
            Command::RootConstants {
                compute: false,
                slot: 1,
                offset: 2,
                values: vec![7, 8],
            },
            Command::RootConstants {
                compute: false,
                slot: 1,
                offset: 0,
                values: vec![5],
            },
            Command::RootView {
                compute: false,
                slot: 0,
                view: RootView::Srv,
                address: 0x1_0000_1000,
            },
            Command::RootTable {
                compute: false,
                slot: 2,
                handle: 0x2_0000_0040,
            },
            // compute arguments are another state
            Command::RootTable {
                compute: true,
                slot: 2,
                handle: 0x9,
            },
        ];
        for c in &cmds[..4] {
            assert!(args.apply(c));
        }
        assert!(!args.apply(&Command::Topology(4)));
        let rows = [
            row(1, 0, 2, 8, 0),
            row(0, 1, 3, 10, 1), // constants 1..4 (4 is unset)
            row(3, 0, 1, 14, 2),
            row(3, 8, 1, 14, 2), // another row of the same SGPR: same values
            row(4, 0, 0, 0, 3),
        ];
        // device = host + 0x100
        let mut tr = |a: u64| Some(a + 0x100);
        let b = fill(&rows, &args, 0xabc0, &mut tr).unwrap();
        assert_eq!((u32_at(&b, 4 * 8), u32_at(&b, 4 * 9)), (0x1000 + 0x100, 1));
        assert_eq!(
            [10, 11, 12].map(|k| u32_at(&b, 4 * k)),
            [0, 7, 8],
            "constants 1, 2, 3: constant 1 was never set"
        );
        assert_eq!(u32_at(&b, 4 * 14), 0x40 + 0x100);
        assert_eq!(u64_at(&b, 128 + 8 * 14), 0x2_0000_0140);
        assert_eq!(u64_at(&b, 384), 0xabc0);
        assert_eq!(BLOCK, 392);
        // a new root signature forgets the arguments
        args.apply(&Command::RootSignature {
            compute: false,
            signature: 1,
        });
        // (the descriptor and the constants read as 0, but a table that is not set fails)
        let Err(FillError::Memory(e)) = fill(&rows, &args, 0, &mut tr) else {
            panic!()
        };
        assert!(e.contains("not set"), "{e}");
        let b = fill(&rows[..2], &args, 0, &mut tr).unwrap();
        assert_eq!(u32_at(&b, 4 * 8), 0);
    }

    #[test]
    fn an_address_that_does_not_translate_fails_the_block() {
        let mut args = RootArgs::default();
        args.apply(&Command::RootView {
            compute: false,
            slot: 0,
            view: RootView::Cbv,
            address: 0x5000,
        });
        let Err(FillError::Memory(e)) = fill(&[row(1, 0, 2, 8, 0)], &args, 0, &mut |_| None) else {
            panic!()
        };
        assert!(e.contains("0x5000"), "{e}");
        let Err(FillError::Layout(e)) = fill(&[row(0, 0, 4, 30, 0)], &args, 0, &mut |_| None)
        else {
            panic!()
        };
        assert!(e.contains("past the root block"), "{e}");
        // two meanings for one user SGPR
        let both = [row(1, 0, 2, 8, 0), row(0, 0, 4, 8, 1)];
        let Err(FillError::Layout(e)) = fill(&both, &args, 0, &mut |a| Some(a)) else {
            panic!()
        };
        assert!(e.contains("user SGPR 8"), "{e}");
    }
}
