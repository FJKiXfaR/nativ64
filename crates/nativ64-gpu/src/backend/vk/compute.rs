//! Dispatches (chunk 10c, `docs/shaders.md`, Chunk 10c design, part 2): `Dispatch` recorded as
//! a compute pipeline bound with the bindless set, the root block's address pushed (the block
//! is filled from the list's compute root arguments and the CS's layout rows, as for a draw)
//! and `vkCmdDispatch`, followed by the global barrier (simple and correct; finer if a profile
//! asks).
//!
//! A dispatch is skipped, counted by reason in `ReplayStats::dispatch_skips` and logged once,
//! when no compute pipeline is bound, the pipeline did not build (the translator refuses
//! most compute programs until chunk 10c part 1 covers them; the reason is its error), an
//! address of the root block is not GPU memory, or the state is otherwise unusable (no
//! bindless set or arena room, a group count above 65535). Mesh dispatches and indirect
//! executions are not replayed (they stay counted in `ReplayStats::dispatches`).
//!
//! Unordered access views (`View::Unordered` of a texture) reach the shader through binding 1
//! of the bindless set, written by `textures.rs` at the view's registry index; buffers written
//! by a CS need nothing here: title memory is imported host memory.

use super::bindless::PUSH_STAGES;
use super::draw::{pipeline_data, Skip};
use super::log_once;
use super::pipeline::Outcome;
use super::replay::{global_barrier, VkBackend};
use super::rootblock::{self, RootArgs};
use crate::d3d12::command::Command;
use ash::{vk, Device};

/// Dispatches skipped, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DispatchSkips {
    /// No pipeline, not a compute pipeline, or one that did not build.
    pub pipeline: u64,
    /// An address of the root block is not GPU memory.
    pub memory: u64,
    /// Anything else (bindless set, arena, group counts).
    pub state: u64,
}

impl DispatchSkips {
    pub fn total(&self) -> u64 {
        self.pipeline + self.memory + self.state
    }
}

/// The most groups of one dimension (the Vulkan minimum for `maxComputeWorkGroupCount`).
const MAX_GROUPS: u32 = 65535;

/// The compute side of a list's state: a fresh `RootArgs` for the compute root signature.
pub(super) fn new_args() -> RootArgs {
    RootArgs::compute()
}

impl VkBackend<'_> {
    /// Records a `Dispatch`.
    pub(super) fn dispatch_command(&mut self, d: &Device, cb: vk::CommandBuffer, c: &Command) {
        let Command::Dispatch { x, y, z, .. } = c else {
            return;
        };
        match self.try_dispatch(d, cb, [*x, *y, *z]) {
            Ok(()) => self.stats.dispatched += 1,
            Err(s) => {
                let (n, why) = match s {
                    Skip::Pipeline(w) => (&mut self.stats.dispatch_skips.pipeline, w),
                    Skip::Memory(w) => (&mut self.stats.dispatch_skips.memory, w),
                    Skip::Target(w) | Skip::State(w) => (&mut self.stats.dispatch_skips.state, w),
                };
                *n += 1;
                log_once(format!("dispatch skipped: {why}"));
            }
        }
    }

    fn try_dispatch(
        &mut self,
        d: &Device,
        cb: vk::CommandBuffer,
        groups: [u32; 3],
    ) -> Result<(), Skip> {
        let ptr = self.draw.list.pipeline;
        if ptr == 0 {
            return Err(Skip::Pipeline("no pipeline bound".into()));
        }
        // SAFETY: the pointer was recorded from a live pipeline object of ours.
        let Some(pd) = (unsafe { pipeline_data(ptr) }) else {
            return Err(Skip::Pipeline("the bound object is not a pipeline".into()));
        };
        if pd.graphics.is_some() {
            return Err(Skip::Pipeline(
                "a graphics pipeline is bound at a dispatch".into(),
            ));
        }
        if self.draw.bindless().is_none() {
            return Err(Skip::State("no bindless descriptor set".into()));
        }
        let draw = &mut self.draw;
        let Some(Some(bl)) = draw.bindless.as_mut() else {
            return Err(Skip::State("no bindless descriptor set".into()));
        };
        let set = bl.set;
        let built = match draw.pipelines.get(bl, &mut draw.samplers, pd, false, &[]) {
            Outcome::Built(b) => b.clone(),
            Outcome::Failed(w) => return Err(Skip::Pipeline(w.clone())),
        };
        if groups.iter().any(|&g| g > MAX_GROUPS) {
            return Err(Skip::State(format!(
                "group counts {groups:?} above {MAX_GROUPS}"
            )));
        }
        let Some(view_table) = self.draw.view_table() else {
            return Err(Skip::State("no view table".into()));
        };
        let mem = &mut self.draw.mem;
        let block = rootblock::fill(&built.rows, &self.draw.list.croot, view_table, &mut |a| {
            mem.translate(a, 1).map(|m| m.address)
        })
        .map_err(|e| match e {
            rootblock::FillError::Memory(w) => Skip::Memory(w),
            rootblock::FillError::Layout(w) => Skip::State(w),
        })?;
        let Some((arena, at)) = self.draw.arena_slot() else {
            return Err(Skip::State("no room for the root block".into()));
        };
        arena.write(at, &block);
        let root_address = arena.address + at as u64;
        if groups.contains(&0) {
            return Ok(());
        }
        let bind = vk::PipelineBindPoint::COMPUTE;
        let layout = built.layouts.layout;
        // SAFETY: `cb` is recording outside a rendering instance (`record` ended it); every
        // handle is live until the submission completes.
        unsafe {
            d.cmd_bind_pipeline(cb, bind, built.pipeline);
            let sets: Vec<vk::DescriptorSet> = [Some(set), built.layouts.set1]
                .into_iter()
                .flatten()
                .collect();
            d.cmd_bind_descriptor_sets(cb, bind, layout, 0, &sets, &[]);
            d.cmd_push_constants(
                cb,
                layout,
                PUSH_STAGES & vk::ShaderStageFlags::COMPUTE,
                0,
                &root_address.to_le_bytes(),
            );
            d.cmd_dispatch(cb, groups[0], groups[1], groups[2]);
        }
        global_barrier(d, cb);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::draw::tests::title_memory;
    use super::super::pipeline::COMPUTE_MODULES;
    use super::super::replay::tests::texture;
    use super::super::tests::gpu_or_skip;
    use super::*;
    use crate::d3d12::command::RootView;
    use crate::d3d12::descriptor::{create_unordered_access_view, registry_index_at};
    use crate::d3d12::pipeline::{create_compute, PIPELINE_IIDS};
    use crate::shader::native::RootLayout;

    /// `glslc --target-env=vulkan1.3` of this (a test fixture standing for what the translator
    /// makes of a compute program): 4x2 groups, the root block through the push constant,
    /// user dwords 0 and 1 a buffer address, dword 2 the index of a storage image.
    ///
    /// ```glsl
    /// layout(local_size_x = 4, local_size_y = 2, local_size_z = 1) in;
    /// layout(buffer_reference, std430) buffer Words { uint w[]; };
    /// layout(buffer_reference, std430) readonly buffer Root { uint user[32]; };
    /// layout(push_constant) uniform P { Root root; } pc;
    /// layout(set = 0, binding = 1) uniform writeonly image2D images[];
    /// void main() {
    ///     uint x = gl_GlobalInvocationID.x, y = gl_GlobalInvocationID.y;
    ///     Words out_ = Words(uvec2(pc.root.user[0], pc.root.user[1]));
    ///     uint lin = y * 8 + x;
    ///     out_.w[lin] = 0xCAFE0000u | lin;
    ///     imageStore(images[nonuniformEXT(pc.root.user[2])], ivec2(x, y),
    ///                vec4(1.0, 0.5, 0.0, 1.0));
    /// }
    /// ```
    const WRITER: &[u32] = &[
        0x07230203, 0x00010600, 0x000d000b, 0x0000005a, 0x00000000, 0x00020011, 0x00000001,
        0x00020011, 0x00000038, 0x00020011, 0x000014b5, 0x00020011, 0x000014b6, 0x00020011,
        0x000014bd, 0x00020011, 0x000014e3, 0x0006000b, 0x00000001, 0x4c534c47, 0x6474732e,
        0x3035342e, 0x00000000, 0x0003000e, 0x000014e4, 0x00000001, 0x0008000f, 0x00000005,
        0x00000004, 0x6e69616d, 0x00000000, 0x0000000b, 0x0000001f, 0x00000046, 0x00060010,
        0x00000004, 0x00000011, 0x00000004, 0x00000002, 0x00000001, 0x00030003, 0x00000002,
        0x000001c2, 0x00070004, 0x455f4c47, 0x625f5458, 0x65666675, 0x65725f72, 0x65726566,
        0x0065636e, 0x00090004, 0x455f4c47, 0x625f5458, 0x65666675, 0x65725f72, 0x65726566,
        0x5f65636e, 0x63657675, 0x00000032, 0x00080004, 0x455f4c47, 0x6e5f5458, 0x6e756e6f,
        0x726f6669, 0x75715f6d, 0x66696c61, 0x00726569, 0x000a0004, 0x475f4c47, 0x4c474f4f,
        0x70635f45, 0x74735f70, 0x5f656c79, 0x656e696c, 0x7269645f, 0x69746365, 0x00006576,
        0x00080004, 0x475f4c47, 0x4c474f4f, 0x6e695f45, 0x64756c63, 0x69645f65, 0x74636572,
        0x00657669, 0x00040005, 0x00000004, 0x6e69616d, 0x00000000, 0x00030005, 0x00000008,
        0x00000078, 0x00080005, 0x0000000b, 0x475f6c67, 0x61626f6c, 0x766e496c, 0x7461636f,
        0x496e6f69, 0x00000044, 0x00030005, 0x00000010, 0x00000079, 0x00040005, 0x00000016,
        0x64726f57, 0x00000073, 0x00040006, 0x00000016, 0x00000000, 0x00000077, 0x00040005,
        0x00000018, 0x5f74756f, 0x00000000, 0x00030005, 0x0000001a, 0x00000050, 0x00050006,
        0x0000001a, 0x00000000, 0x746f6f72, 0x00000000, 0x00040005, 0x0000001d, 0x746f6f52,
        0x00000000, 0x00050006, 0x0000001d, 0x00000000, 0x72657375, 0x00000000, 0x00030005,
        0x0000001f, 0x00006370, 0x00030005, 0x00000030, 0x00786469, 0x00030005, 0x00000036,
        0x006e696c, 0x00040005, 0x00000046, 0x67616d69, 0x00007365, 0x00040047, 0x0000000b,
        0x0000000b, 0x0000001c, 0x00040047, 0x00000015, 0x00000006, 0x00000004, 0x00030047,
        0x00000016, 0x00000002, 0x00050048, 0x00000016, 0x00000000, 0x00000023, 0x00000000,
        0x00030047, 0x00000018, 0x000014ec, 0x00030047, 0x0000001a, 0x00000002, 0x00050048,
        0x0000001a, 0x00000000, 0x00000023, 0x00000000, 0x00040047, 0x0000001c, 0x00000006,
        0x00000004, 0x00030047, 0x0000001d, 0x00000002, 0x00040048, 0x0000001d, 0x00000000,
        0x00000018, 0x00050048, 0x0000001d, 0x00000000, 0x00000023, 0x00000000, 0x00030047,
        0x00000046, 0x00000019, 0x00040047, 0x00000046, 0x00000021, 0x00000001, 0x00040047,
        0x00000046, 0x00000022, 0x00000000, 0x00030047, 0x00000048, 0x000014b4, 0x00030047,
        0x0000004a, 0x000014b4, 0x00030047, 0x0000004b, 0x000014b4, 0x00020013, 0x00000002,
        0x00030021, 0x00000003, 0x00000002, 0x00040015, 0x00000006, 0x00000020, 0x00000000,
        0x00040020, 0x00000007, 0x00000007, 0x00000006, 0x00040017, 0x00000009, 0x00000006,
        0x00000003, 0x00040020, 0x0000000a, 0x00000001, 0x00000009, 0x0004003b, 0x0000000a,
        0x0000000b, 0x00000001, 0x0004002b, 0x00000006, 0x0000000c, 0x00000000, 0x00040020,
        0x0000000d, 0x00000001, 0x00000006, 0x0004002b, 0x00000006, 0x00000011, 0x00000001,
        0x00030027, 0x00000014, 0x000014e5, 0x0003001d, 0x00000015, 0x00000006, 0x0003001e,
        0x00000016, 0x00000015, 0x00040020, 0x00000014, 0x000014e5, 0x00000016, 0x00040020,
        0x00000017, 0x00000007, 0x00000014, 0x00030027, 0x00000019, 0x000014e5, 0x0003001e,
        0x0000001a, 0x00000019, 0x0004002b, 0x00000006, 0x0000001b, 0x00000020, 0x0004001c,
        0x0000001c, 0x00000006, 0x0000001b, 0x0003001e, 0x0000001d, 0x0000001c, 0x00040020,
        0x00000019, 0x000014e5, 0x0000001d, 0x00040020, 0x0000001e, 0x00000009, 0x0000001a,
        0x0004003b, 0x0000001e, 0x0000001f, 0x00000009, 0x00040015, 0x00000020, 0x00000020,
        0x00000001, 0x0004002b, 0x00000020, 0x00000021, 0x00000000, 0x00040020, 0x00000022,
        0x00000009, 0x00000019, 0x00040020, 0x00000025, 0x000014e5, 0x00000006, 0x0004002b,
        0x00000020, 0x0000002a, 0x00000001, 0x00040017, 0x0000002d, 0x00000006, 0x00000002,
        0x0004002b, 0x00000020, 0x00000033, 0x00000002, 0x0004002b, 0x00000006, 0x00000038,
        0x00000008, 0x0004002b, 0x00000006, 0x0000003e, 0xcafe0000, 0x00030016, 0x00000042,
        0x00000020, 0x00090019, 0x00000043, 0x00000042, 0x00000001, 0x00000000, 0x00000000,
        0x00000000, 0x00000002, 0x00000000, 0x0003001d, 0x00000044, 0x00000043, 0x00040020,
        0x00000045, 0x00000000, 0x00000044, 0x0004003b, 0x00000045, 0x00000046, 0x00000000,
        0x00040020, 0x00000049, 0x00000000, 0x00000043, 0x00040017, 0x00000050, 0x00000020,
        0x00000002, 0x00040017, 0x00000052, 0x00000042, 0x00000004, 0x0004002b, 0x00000042,
        0x00000053, 0x3f800000, 0x0004002b, 0x00000042, 0x00000054, 0x3f000000, 0x0004002b,
        0x00000042, 0x00000055, 0x00000000, 0x0007002c, 0x00000052, 0x00000056, 0x00000053,
        0x00000054, 0x00000055, 0x00000053, 0x0004002b, 0x00000006, 0x00000057, 0x00000004,
        0x0004002b, 0x00000006, 0x00000058, 0x00000002, 0x0006002c, 0x00000009, 0x00000059,
        0x00000057, 0x00000058, 0x00000011, 0x00050036, 0x00000002, 0x00000004, 0x00000000,
        0x00000003, 0x000200f8, 0x00000005, 0x0004003b, 0x00000007, 0x00000008, 0x00000007,
        0x0004003b, 0x00000007, 0x00000010, 0x00000007, 0x0004003b, 0x00000017, 0x00000018,
        0x00000007, 0x0004003b, 0x00000007, 0x00000030, 0x00000007, 0x0004003b, 0x00000007,
        0x00000036, 0x00000007, 0x00050041, 0x0000000d, 0x0000000e, 0x0000000b, 0x0000000c,
        0x0004003d, 0x00000006, 0x0000000f, 0x0000000e, 0x0003003e, 0x00000008, 0x0000000f,
        0x00050041, 0x0000000d, 0x00000012, 0x0000000b, 0x00000011, 0x0004003d, 0x00000006,
        0x00000013, 0x00000012, 0x0003003e, 0x00000010, 0x00000013, 0x00050041, 0x00000022,
        0x00000023, 0x0000001f, 0x00000021, 0x0004003d, 0x00000019, 0x00000024, 0x00000023,
        0x00060041, 0x00000025, 0x00000026, 0x00000024, 0x00000021, 0x00000021, 0x0006003d,
        0x00000006, 0x00000027, 0x00000026, 0x00000002, 0x00000004, 0x00050041, 0x00000022,
        0x00000028, 0x0000001f, 0x00000021, 0x0004003d, 0x00000019, 0x00000029, 0x00000028,
        0x00060041, 0x00000025, 0x0000002b, 0x00000029, 0x00000021, 0x0000002a, 0x0006003d,
        0x00000006, 0x0000002c, 0x0000002b, 0x00000002, 0x00000004, 0x00050050, 0x0000002d,
        0x0000002e, 0x00000027, 0x0000002c, 0x0004007c, 0x00000014, 0x0000002f, 0x0000002e,
        0x0003003e, 0x00000018, 0x0000002f, 0x00050041, 0x00000022, 0x00000031, 0x0000001f,
        0x00000021, 0x0004003d, 0x00000019, 0x00000032, 0x00000031, 0x00060041, 0x00000025,
        0x00000034, 0x00000032, 0x00000021, 0x00000033, 0x0006003d, 0x00000006, 0x00000035,
        0x00000034, 0x00000002, 0x00000004, 0x0003003e, 0x00000030, 0x00000035, 0x0004003d,
        0x00000006, 0x00000037, 0x00000010, 0x00050084, 0x00000006, 0x00000039, 0x00000037,
        0x00000038, 0x0004003d, 0x00000006, 0x0000003a, 0x00000008, 0x00050080, 0x00000006,
        0x0000003b, 0x00000039, 0x0000003a, 0x0003003e, 0x00000036, 0x0000003b, 0x0004003d,
        0x00000014, 0x0000003c, 0x00000018, 0x0004003d, 0x00000006, 0x0000003d, 0x00000036,
        0x0004003d, 0x00000006, 0x0000003f, 0x00000036, 0x000500c5, 0x00000006, 0x00000040,
        0x0000003e, 0x0000003f, 0x00060041, 0x00000025, 0x00000041, 0x0000003c, 0x00000021,
        0x0000003d, 0x0005003e, 0x00000041, 0x00000040, 0x00000002, 0x00000004, 0x0004003d,
        0x00000006, 0x00000047, 0x00000030, 0x00040053, 0x00000006, 0x00000048, 0x00000047,
        0x00050041, 0x00000049, 0x0000004a, 0x00000046, 0x00000048, 0x0004003d, 0x00000043,
        0x0000004b, 0x0000004a, 0x0004003d, 0x00000006, 0x0000004c, 0x00000008, 0x0004007c,
        0x00000020, 0x0000004d, 0x0000004c, 0x0004003d, 0x00000006, 0x0000004e, 0x00000010,
        0x0004007c, 0x00000020, 0x0000004f, 0x0000004e, 0x00050050, 0x00000050, 0x00000051,
        0x0000004d, 0x0000004f, 0x00040063, 0x0000004b, 0x00000051, 0x00000056, 0x000100fd,
        0x00010038,
    ];

    fn row(root_type: u32, count: u32, first: u8, slot: u8) -> RootLayout {
        RootLayout {
            root_type,
            table_offset: 0,
            user_sgprs: count,
            base_register: 0,
            first_user_dword: first,
            root_slot: slot,
        }
    }

    /// A compute pipeline object whose translation is `module` (None: the blob is garbage and
    /// the real translator decides).
    fn compute_pipeline(module: Option<&[u32]>) -> usize {
        let blob: &'static [u8] = Vec::leak(vec![0u8; 16]);
        let mut desc = [0u8; 24];
        desc[8..16].copy_from_slice(&(blob.as_ptr() as u64).to_le_bytes());
        desc[16..24].copy_from_slice(&(blob.len() as u64).to_le_bytes());
        let mut out = std::ptr::null_mut();
        // SAFETY: a valid description and out pointer.
        let hr = unsafe {
            create_compute(
                std::ptr::null_mut(),
                desc.as_ptr(),
                &PIPELINE_IIDS[0],
                &mut out,
            )
        };
        assert_eq!(hr, 0);
        let ptr = out as usize;
        if let Some(words) = module {
            // SAFETY: the object was just made.
            let serial = unsafe { pipeline_data(ptr) }.expect("a pipeline").serial;
            let rows = vec![row(1, 2, 0, 0), row(0, 1, 2, 1)];
            COMPUTE_MODULES.with(|m| m.borrow_mut().insert(serial, (words.to_vec(), rows)));
        }
        ptr
    }

    /// The commands of a dispatch of two groups: the output buffer `out` (root parameter 0),
    /// the registry index of the storage image (parameter 1), the table that holds its view.
    fn dispatch(pipe: usize, out: u64, index: u32, table: u64) -> Vec<Command> {
        vec![
            Command::SetPipeline { pipeline: pipe },
            Command::RootView {
                compute: true,
                slot: 0,
                view: RootView::Uav,
                address: out,
            },
            Command::RootConstants {
                compute: true,
                slot: 1,
                offset: 0,
                values: vec![index],
            },
            Command::RootTable {
                compute: true,
                slot: 2,
                handle: table,
            },
            Command::Dispatch {
                x: 2,
                y: 1,
                z: 1,
                flags: 0,
            },
        ]
    }

    /// An 8x2 RGBA8 texture (with `flags`), a UAV of it in a table, and the index of the view.
    fn unordered(flags: u32) -> (usize, u64, u32) {
        let tex = texture(8, 2, 28, flags);
        let table = title_memory(&[0u8; 256]);
        // SAFETY: a live resource, no description, a slot in title memory.
        let index = unsafe {
            create_unordered_access_view(
                std::ptr::null_mut(),
                tex as *mut _,
                std::ptr::null_mut(),
                std::ptr::null(),
                table,
            );
            registry_index_at(table)
        };
        (tex, table, index.expect("a registered view"))
    }

    fn words_at(address: u64, n: usize) -> Vec<u32> {
        // SAFETY: title memory of at least `n` words that the test made.
        unsafe { std::slice::from_raw_parts(address as *const u32, n) }.to_vec()
    }

    #[test]
    fn a_dispatch_writes_a_buffer_and_a_storage_image_and_owns_the_image() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let (tex, table, index) = unordered(4);
        let out = title_memory(&[0u8; 64]);
        let pipe = compute_pipeline(Some(WRITER));
        b.execute(&[dispatch(pipe, out, index, table)]);
        assert_eq!(
            (
                b.stats.dispatches,
                b.stats.dispatched,
                b.stats.dispatch_skips.total()
            ),
            (1, 1, 0),
            "{:?}",
            b.stats
        );
        let want: Vec<u32> = (0..16).map(|i| 0xCAFE_0000 | i).collect();
        assert_eq!(words_at(out, 16), want, "the root buffer");
        let r = b.read_back(tex).expect("the image");
        assert_eq!((r.width, r.height), (8, 2));
        for p in r.bytes.chunks(4) {
            assert_eq!((p[0], p[2], p[3]), (255, 0, 255), "{:?}", r.bytes);
            assert!((127..=128).contains(&p[1]), "{:?}", r.bytes);
        }
        let t = b.images[&tex].as_ref().unwrap();
        assert!(
            t.storage && t.gpu_owned,
            "STORAGE usage, and the GPU's from now on"
        );
        // a second submission keeps working (the descriptor and the views are reused)
        b.execute(&[dispatch(pipe, out, index, table)]);
        assert_eq!((b.stats.dispatched, b.stats.dispatch_skips.total()), (2, 0));
    }

    #[test]
    fn an_image_without_unordered_access_reads_the_storage_hole() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let (tex, table, index) = unordered(0);
        let out = title_memory(&[0u8; 64]);
        b.execute(&[dispatch(compute_pipeline(Some(WRITER)), out, index, table)]);
        assert_eq!(b.stats.dispatched, 1, "{:?}", b.stats);
        assert_eq!(
            words_at(out, 2),
            [0xCAFE_0000, 0xCAFE_0001],
            "the buffer is written"
        );
        assert!(!b.images[&tex].as_ref().unwrap().storage);
    }

    #[test]
    fn dispatches_that_cannot_be_replayed_are_counted_by_reason() {
        let Some(g) = gpu_or_skip() else { return };
        let mut b = VkBackend::new(&g);
        let (_, table, index) = unordered(4);
        let go = Command::Dispatch {
            x: 1,
            y: 1,
            z: 1,
            flags: 0,
        };
        // no pipeline
        b.execute(&[vec![go]]);
        // a pipeline the translator does not make a module of (a garbage blob)
        b.execute(&[dispatch(compute_pipeline(None), 0, index, table)]);
        // a root buffer that is not GPU memory
        let ok = compute_pipeline(Some(WRITER));
        b.execute(&[dispatch(ok, 0x10, index, table)]);
        // too many groups
        let mut huge = dispatch(ok, title_memory(&[0u8; 64]), index, table);
        huge.pop();
        huge.push(Command::Dispatch {
            x: 70000,
            y: 1,
            z: 1,
            flags: 0,
        });
        b.execute(&[huge]);
        let s = b.stats.dispatch_skips;
        assert_eq!(
            (
                b.stats.dispatches,
                b.stats.dispatched,
                s.pipeline,
                s.memory,
                s.state
            ),
            (4, 0, 2, 1, 1),
            "{:?}",
            b.stats
        );
    }

    #[test]
    fn compute_and_graphics_root_arguments_are_apart() {
        let mut gfx = RootArgs::default();
        let mut cs = RootArgs::compute();
        for compute in [false, true] {
            let c = Command::RootTable {
                compute,
                slot: 1,
                handle: if compute { 0xc0 } else { 0x90 },
            };
            assert!(gfx.apply(&c) && cs.apply(&c));
        }
        assert_eq!((gfx.tables[&1], cs.tables[&1]), (0x90, 0xc0));
        let sig = |compute| Command::RootSignature {
            compute,
            signature: 7,
        };
        cs.apply(&sig(false));
        assert_eq!(
            cs.tables.len(),
            1,
            "a graphics signature leaves the compute arguments"
        );
        cs.apply(&sig(true));
        assert!(cs.tables.is_empty() && gfx.tables.len() == 1);
    }
}
