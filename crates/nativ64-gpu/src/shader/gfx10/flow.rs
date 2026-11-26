//! Flow following: the instructions reachable from an entry point.

use super::{decode_wave, DecodeError, ErrorKind, Instr};
use std::collections::BTreeMap;

/// The instructions reachable from the entry, in address order.
#[derive(Debug)]
pub struct Program {
    pub instrs: Vec<Instr>,
    /// Offsets that start a basic block: the entry, branch targets and the instruction after
    /// a conditional branch.
    pub leaders: Vec<usize>,
    /// Offsets of jumps through registers (`s_setpc_b64`, `s_swappc_b64`, `s_rfe_b64`) and
    /// `s_call_b64`: the translator stops at these.
    pub indirect: Vec<usize>,
}

/// What the caller knows about an indirect instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A jump (`s_setpc_b64`) to this byte offset.
    Jump(usize),
    /// A call that returns to the next instruction (the vertex fetch routine).
    Call,
}

/// Follows control flow from byte offset `entry`: fall-through, `s_branch`, `s_cbranch_*`
/// (both edges); `s_endpgm` ends a path. A target outside `code`, or inside an instruction
/// already decoded, is an error; so is any undecodable reachable instruction.
pub fn follow(code: &[u8], entry: usize, wave32: bool) -> Result<Program, DecodeError> {
    follow_with(code, entry, wave32, &BTreeMap::new())
}

/// Like `follow`, with the indirect instructions at the offsets of `resolved` followed as the
/// jump or call they were found to be (they no longer appear in `indirect`).
pub fn follow_with(
    code: &[u8],
    entry: usize,
    wave32: bool,
    resolved: &BTreeMap<usize, Resolved>,
) -> Result<Program, DecodeError> {
    let mut seen: BTreeMap<usize, Instr> = BTreeMap::new();
    let mut leaders = vec![entry];
    let mut indirect = Vec::new();
    let mut work = vec![entry];
    while let Some(start) = work.pop() {
        let mut at = start;
        loop {
            if seen.contains_key(&at) {
                break;
            }
            let i = decode_wave(code, at, wave32)?;
            let next = at + i.len;
            let mut stop = i.is_endpgm();
            let mut target = i.branch_target().filter(|_| !i.is_indirect());
            if i.is_indirect() {
                match resolved.get(&at) {
                    Some(Resolved::Call) => {}
                    Some(Resolved::Jump(t)) => {
                        target = Some(*t as i64);
                        stop = true;
                    }
                    None => {
                        indirect.push(at);
                        stop = true;
                    }
                }
            }
            if let Some(t) = target {
                if t < 0 || t as usize >= code.len() || t % 4 != 0 {
                    return Err(DecodeError {
                        at,
                        kind: ErrorKind::BadTarget(t),
                    });
                }
                leaders.push(t as usize);
                work.push(t as usize);
                if i.is_branch() || i.is_indirect() {
                    stop = true;
                } else if i.is_cbranch() {
                    leaders.push(next);
                }
            }
            seen.insert(at, i);
            if stop {
                break;
            }
            at = next;
        }
    }
    // a target in the middle of an instruction decoded from another path
    let mut prev_end = 0;
    for (&at, i) in &seen {
        if at < prev_end {
            return Err(DecodeError {
                at,
                kind: ErrorKind::BadTarget(at as i64),
            });
        }
        prev_end = at + i.len;
    }
    leaders.sort_unstable();
    leaders.dedup();
    indirect.sort_unstable();
    Ok(Program {
        instrs: seen.into_values().collect(),
        leaders,
        indirect,
    })
}
