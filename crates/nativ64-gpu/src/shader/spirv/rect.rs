//! The geometry shader of the console's rectangle lists (chunk 10b, part 3, `docs/shaders.md`).
//!
//! A rectangle comes as three corners in any order; the hardware adds the fourth, opposite the
//! right angle, at `va + vb - vc` for the position and every attribute. Vulkan has no such
//! primitive, so this module emits, from the vertex shader's output interface, a geometry shader
//! that takes each triangle `(v0, v1, v2)`, finds the right angle (the corner opposite the
//! longest side in post-divide x, y, else corner 1) and emits the triangle itself and its
//! completion `(va, vb, v3)` (`va` before the right angle, `vb` after it) as two strips of one
//! triangle, so both turn the same way. Flat outputs keep `va`'s value at the fourth corner.

use spirv::{Capability, Decoration, ExecutionMode, ExecutionModel, Op, StorageClass};

use super::builder::{inst, Module};

/// The vertex shader's outputs the geometry shader passes on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RectInterface {
    /// Every `Location` output (a `vec4` of `f32`), with whether it is flat.
    pub locations: Vec<(u32, bool)>,
    /// The vertex shader writes `ClipDistance` (eight floats).
    pub clip: bool,
}

/// Which of a rectangle's three corners (post-divide x and y of each) holds the right angle:
/// the one opposite the longest side, else (ties, NaN) `1`. The geometry shader makes the same
/// choice on the GPU.
pub fn right_angle(xy: [[f32; 2]; 3]) -> usize {
    let side = |i: usize, j: usize| {
        let (dx, dy) = (xy[i][0] - xy[j][0], xy[i][1] - xy[j][1]);
        dx * dx + dy * dy
    };
    let (d12, d20, d01) = (side(1, 2), side(2, 0), side(0, 1));
    if d12 > d20 && d12 > d01 {
        0
    } else if d01 > d12 && d01 > d20 {
        2
    } else {
        1
    }
}

/// One passed-through output: its variables and its scalar count.
struct Item {
    input: u32,
    output: u32,
    comps: u32,
    flat: bool,
}

/// The function body under construction.
struct Body {
    m: Module,
    code: Vec<u32>,
}

impl Body {
    fn op(&mut self, op: Op, ty: u32, args: &[u32]) -> u32 {
        let id = self.m.id();
        let mut ops = vec![ty, id];
        ops.extend_from_slice(args);
        self.code.extend(inst(op, &ops));
        id
    }

    fn effect(&mut self, op: Op, args: &[u32]) {
        self.code.extend(inst(op, args));
    }
}

/// The geometry shader (SPIR-V words) for a vertex shader with this output interface.
pub fn rectangle_gs(iface: &RectInterface) -> Vec<u32> {
    let mut m = Module::new();
    m.capability(Capability::Shader);
    m.capability(Capability::Geometry);
    if iface.clip {
        m.capability(Capability::ClipDistance);
    }
    let void = m.global(Op::TypeVoid, None, &[]);
    let boolean = m.global(Op::TypeBool, None, &[]);
    let u32t = m.global(Op::TypeInt, None, &[32, 0]);
    let f32t = m.global(Op::TypeFloat, None, &[32]);
    let vec4 = m.global(Op::TypeVector, None, &[f32t, 4]);
    let konst = |m: &mut Module, x: u32| m.global(Op::Constant, Some(u32t), &[x]);
    let (k3, k8) = (konst(&mut m, 3), konst(&mut m, 8));
    let clip_arr = m.global(Op::TypeArray, None, &[f32t, k8]);
    let ptr = |m: &mut Module, class: StorageClass, t: u32| {
        m.global(Op::TypePointer, None, &[class as u32, t])
    };
    let pin_f = ptr(&mut m, StorageClass::Input, f32t);
    let pout_f = ptr(&mut m, StorageClass::Output, f32t);

    // (type of one corner's value, scalar count, built-in or location, flat)
    let mut specs: Vec<(u32, u32, Result<spirv::BuiltIn, u32>, bool)> =
        vec![(vec4, 4, Ok(spirv::BuiltIn::Position), false)];
    if iface.clip {
        specs.push((clip_arr, 8, Ok(spirv::BuiltIn::ClipDistance), false));
    }
    for &(loc, flat) in &iface.locations {
        specs.push((vec4, 4, Err(loc), flat));
    }
    let mut iface_ids = Vec::new();
    let mut items = Vec::new();
    for (ty, comps, which, flat) in specs {
        let in_arr = m.global(Op::TypeArray, None, &[ty, k3]);
        let pin = ptr(&mut m, StorageClass::Input, in_arr);
        let pout = ptr(&mut m, StorageClass::Output, ty);
        let input = m.global_var(pin, StorageClass::Input, None);
        let output = m.global_var(pout, StorageClass::Output, None);
        for v in [input, output] {
            match which {
                Ok(b) => m.decorate(v, Decoration::BuiltIn, &[b as u32]),
                Err(loc) => m.decorate(v, Decoration::Location, &[loc]),
            }
        }
        if flat {
            m.decorate(output, Decoration::Flat, &[]);
        }
        iface_ids.extend([input, output]);
        items.push(Item {
            input,
            output,
            comps,
            flat,
        });
    }

    let fty = m.global(Op::TypeFunction, None, &[void]);
    let main = m.id();
    let first = m.id();
    // The indices of the access chains (components 0..8, corners 0..3) are constants.
    let ks: Vec<u32> = (0..8).map(|x| konst(&mut m, x)).collect();
    let mut b = Body {
        m,
        code: Vec::new(),
    };
    let mut vals: Vec<Vec<Vec<u32>>> = Vec::new(); // [corner][item][component]
    for k in 0..3 {
        let mut per_item = Vec::new();
        for it in &items {
            let mut comps = Vec::new();
            for c in 0..it.comps {
                let p = b.op(Op::AccessChain, pin_f, &[it.input, ks[k], ks[c as usize]]);
                comps.push(b.op(Op::Load, f32t, &[p]));
            }
            per_item.push(comps);
        }
        vals.push(per_item);
    }
    // The right angle from the post-divide position, as [`right_angle`] picks it.
    let mut xy = Vec::new();
    for corner in &vals {
        let pos = &corner[0];
        let x = b.op(Op::FDiv, f32t, &[pos[0], pos[3]]);
        let y = b.op(Op::FDiv, f32t, &[pos[1], pos[3]]);
        xy.push([x, y]);
    }
    let side = |b: &mut Body, i: usize, j: usize| {
        let dx = b.op(Op::FSub, f32t, &[xy[i][0], xy[j][0]]);
        let dy = b.op(Op::FSub, f32t, &[xy[i][1], xy[j][1]]);
        let xx = b.op(Op::FMul, f32t, &[dx, dx]);
        let yy = b.op(Op::FMul, f32t, &[dy, dy]);
        b.op(Op::FAdd, f32t, &[xx, yy])
    };
    let (d12, d20, d01) = (side(&mut b, 1, 2), side(&mut b, 2, 0), side(&mut b, 0, 1));
    let longest = |b: &mut Body, d: u32, o: [u32; 2]| {
        let x = b.op(Op::FOrdGreaterThan, boolean, &[d, o[0]]);
        let y = b.op(Op::FOrdGreaterThan, boolean, &[d, o[1]]);
        b.op(Op::LogicalAnd, boolean, &[x, y])
    };
    let at0 = longest(&mut b, d12, [d20, d01]);
    let at2 = longest(&mut b, d01, [d12, d20]);
    // `x0` if the right angle is at v0, `x2` if at v2, else `x1`.
    let pick = |b: &mut Body, [x0, x1, x2]: [u32; 3]| {
        let x = b.op(Op::Select, f32t, &[at2, x2, x1]);
        b.op(Op::Select, f32t, &[at0, x0, x])
    };
    // Corners 3, 4, 5 are (va, vb, v3).
    let mut second: [Vec<Vec<u32>>; 3] = Default::default();
    for (n, it) in items.iter().enumerate() {
        let mut cols: [Vec<u32>; 3] = Default::default();
        let [c0, c1, c2] = [0, 1, 2].map(|k| &vals[k][n]);
        for ((&v0, &v1), &v2) in c0.iter().zip(c1).zip(c2) {
            let va = pick(&mut b, [v2, v0, v1]);
            let vb = pick(&mut b, [v1, v2, v0]);
            let v3 = if it.flat {
                va
            } else {
                let vc = pick(&mut b, [v0, v1, v2]);
                let sum = b.op(Op::FAdd, f32t, &[va, vb]);
                b.op(Op::FSub, f32t, &[sum, vc])
            };
            for (k, v) in [va, vb, v3].into_iter().enumerate() {
                cols[k].push(v);
            }
        }
        for (k, c) in cols.into_iter().enumerate() {
            second[k].push(c);
        }
    }
    vals.extend(second);
    for triangle in [[0, 1, 2], [3, 4, 5]] {
        for k in triangle {
            for (n, it) in items.iter().enumerate() {
                for c in 0..it.comps as usize {
                    let p = b.op(Op::AccessChain, pout_f, &[it.output, ks[c]]);
                    b.effect(Op::Store, &[p, vals[k][n][c]]);
                }
            }
            b.effect(Op::EmitVertex, &[]);
        }
        b.effect(Op::EndPrimitive, &[]);
    }
    b.effect(Op::Return, &[]);

    let mut f = inst(Op::Function, &[void, main, 0, fty]);
    f.extend(inst(Op::Label, &[first]));
    f.extend(b.code);
    f.extend(inst(Op::FunctionEnd, &[]));
    let mut m = b.m;
    m.add_function(&f);
    m.entry_point(ExecutionModel::Geometry, main, "main", &iface_ids);
    m.exec_mode(main, ExecutionMode::Triangles, &[]);
    m.exec_mode(main, ExecutionMode::OutputTriangleStrip, &[]);
    m.exec_mode(main, ExecutionMode::OutputVertices, &[6]);
    // Without it the invocation count reads 0, which the spec forbids (and drivers may treat as
    // "no invocation": nothing is drawn).
    m.exec_mode(main, ExecutionMode::Invocations, &[1]);
    m.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader::spirv::tests::validate;

    const CORNERS: [[f32; 2]; 3] = [[0.0, 0.0], [4.0, 0.0], [4.0, 3.0]];

    /// The corners of [`CORNERS`] in each order: the right angle follows the point, not the slot.
    #[test]
    fn the_right_angle_is_found_in_every_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let xy = order.map(|i| CORNERS[i]);
            let at = right_angle(xy);
            assert_eq!(order[at], 1, "order {order:?}");
        }
    }

    /// Ties and NaN keep the blits' corner, `v1`.
    #[test]
    fn degenerate_rectangles_take_corner_one() {
        assert_eq!(right_angle([[0.0, 0.0]; 3]), 1);
        assert_eq!(right_angle([[0.0, 0.0], [1.0, 0.0], [0.5, 0.866_025_4]]), 1);
        assert_eq!(right_angle([[f32::NAN, 0.0], [1.0, 0.0], [1.0, 1.0]]), 1);
    }

    #[test]
    fn the_generated_geometry_shader_validates() {
        for (clip, locations) in [
            (false, vec![]),
            (false, vec![(0, false), (3, true)]),
            (true, vec![(1, false), (2, true)]),
        ] {
            let words = rectangle_gs(&RectInterface { locations, clip });
            validate(&words);
        }
    }

    /// Vulkan wants an invocation count of at least 1 on a geometry stage (VUID 00715); a
    /// shader without the mode reads 0 and the validation layer rejects it.
    #[test]
    fn the_geometry_shader_declares_one_invocation() {
        let words = rectangle_gs(&RectInterface {
            locations: vec![(0, false)],
            clip: false,
        });
        // OpExecutionMode (op 16, 3 words with one literal): entry, mode, value
        let modes: Vec<(u32, u32)> = words[5..]
            .windows(4)
            .filter(|w| w[0] == (4 << 16 | 16))
            .map(|w| (w[2], w[3]))
            .collect();
        let invocations = ExecutionMode::Invocations as u32;
        assert!(modes.contains(&(invocations, 1)), "{modes:?}");
    }
}
