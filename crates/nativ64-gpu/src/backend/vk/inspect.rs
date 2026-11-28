//! `NATIV64_DUMP_TARGETS=<dir>`: with it, the frames the dump keeps (`present::dump_wanted`)
//! also get one `inspect:` line on stderr per replayed draw (pipeline, targets, viewport,
//! scissor, fixed state, root arguments and the first bytes of the memory they point to) and,
//! after the present, every image the replay holds as `<dir>/frame_NNNNN_res<id>_<w>x<h>.png`
//! (render targets and textures alike; formats the PNG dump does not convert are named in the
//! log instead). A debugging aid: it reads back every image, so it is slow.
//!
//! `NATIV64_DUMP_DRAWS=<frame>` (with `NATIV64_DUMP_TARGETS`): that one presented frame also
//! gets its draw `inspect:` lines, and the colour target bound by each draw (and dispatch) is
//! copied after it and written as `<dir>/frame_NNNNN_draw_DDD_p<pipeline>_i<count>.png`.

use super::log_once;
use super::present::{rgba8, write_png};
use super::replay::VkBackend;
use crate::d3d12::command::Command;
use crate::d3d12::descriptor::{TargetRecord, View};
use crate::d3d12::pipeline::PipelineData;
use crate::d3d12::pipeline_state::fixed_state;
use ash::{vk, Device};
use std::path::PathBuf;
use std::sync::OnceLock;

/// `NATIV64_DUMP_DRAWS` parsed: the presented frame whose draws are dumped one by one.
fn parse_draws_frame(v: Option<&str>) -> Option<u64> {
    v?.trim().parse().ok().filter(|&f| f > 0)
}

/// The frame of `NATIV64_DUMP_DRAWS`, when set (needs `NATIV64_DUMP_TARGETS`).
pub fn draws_frame() -> Option<u64> {
    static F: OnceLock<Option<u64>> = OnceLock::new();
    *F.get_or_init(|| {
        dir()?;
        parse_draws_frame(std::env::var("NATIV64_DUMP_DRAWS").ok().as_deref())
    })
}

/// Most snapshots and bytes one frame's draw dump keeps before writing them out.
const SNAP_MAX: usize = 96;
const SNAP_BYTES: u64 = 2 << 30;

/// Whether one more snapshot of `size` bytes fits after `n` snapshots holding `held` bytes.
fn snap_fits(n: usize, held: u64, size: u64) -> bool {
    n < SNAP_MAX && held + size <= SNAP_BYTES
}

/// One colour target copied after a draw, waiting for the submission to finish.
struct Snap {
    name: String,
    width: u32,
    height: u32,
    format: vk::Format,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
}

/// The per-draw dump of one frame (`NATIV64_DUMP_DRAWS`); empty, and nothing allocated, otherwise.
#[derive(Default)]
pub(super) struct DrawDump {
    /// Draws seen in the frame being recorded.
    count: u32,
    /// Set by a recorded draw: (pipeline serial, index or vertex count, target resource).
    fresh: Option<(u64, u32, usize)>,
    last_target: Option<usize>,
    snaps: Vec<Snap>,
    held: u64,
}

/// The directory of `NATIV64_DUMP_TARGETS`, when set and not empty.
pub fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("NATIV64_DUMP_TARGETS")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    })
    .as_ref()
}

/// `n` bytes at `address` as hex, 16 bytes per group (the caller checked the memory).
///
/// # Safety
/// `address..address + n` is readable.
pub unsafe fn hex(address: u64, n: usize) -> String {
    // SAFETY: by the contract.
    let b = unsafe { std::slice::from_raw_parts(address as *const u8, n) };
    b.chunks(16)
        .map(|c| c.iter().map(|x| format!("{x:02x}")).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

impl VkBackend<'_> {
    /// Whether the draws of the frame being recorded are described.
    pub(super) fn inspecting(&self) -> bool {
        dir().is_some()
            && (super::present::dump_wanted(self.frames + 1)
                || draws_frame() == Some(self.frames + 1))
    }

    /// Whether the draws of the frame being recorded are dumped one by one.
    pub(super) fn dumping_draws(&self) -> bool {
        draws_frame() == Some(self.frames + 1)
    }

    /// A draw of the frame was recorded on `target` (called by the draw path).
    pub(super) fn note_draw(&mut self, c: &Command, serial: u64, target: Option<usize>) {
        if !self.dumping_draws() {
            return;
        }
        let n = match c {
            Command::Draw { vertices, .. } => *vertices,
            Command::DrawIndexed { indices, .. } => *indices,
            _ => 0,
        };
        if let Some(t) = target {
            self.dd.fresh = Some((serial, n, t));
            self.dd.last_target = Some(t);
        }
    }

    /// After a command of the frame is recorded: for a draw or dispatch, copy its colour target
    /// into a host buffer (read after the submission by `finish_draw_dump`).
    pub(super) fn snap_after(&mut self, d: &Device, cb: vk::CommandBuffer, c: &Command) {
        if !self.dumping_draws() {
            return;
        }
        let fresh = self.dd.fresh.take();
        let (pipe, count, res) = match c {
            Command::Draw { .. } | Command::DrawIndexed { .. } => {
                let Some(f) = fresh else {
                    eprintln!("inspect: draw {} not drawn (skipped)", self.dd.count);
                    self.dd.count += 1;
                    return;
                };
                f
            }
            Command::Dispatch { .. } => {
                let Some(t) = self.dd.last_target else { return };
                (0, 0, t)
            }
            _ => return,
        };
        let idx = self.dd.count;
        self.dd.count += 1;
        let Some((image, w, h)) = self.colour_image(res) else {
            return;
        };
        let Some(info) = self
            .images
            .get(&res)
            .and_then(Option::as_ref)
            .map(|t| t.info)
        else {
            return;
        };
        let size = u64::from(w) * u64::from(h) * u64::from(info.bytes);
        if !snap_fits(self.dd.snaps.len(), self.dd.held, size) {
            eprintln!("inspect: draw {idx} not kept (snapshot cap)");
            return;
        }
        let kind = if matches!(c, Command::Dispatch { .. }) {
            "disp"
        } else {
            "draw"
        };
        let name = format!(
            "frame_{:05}_draw_{idx:03}_{kind}_p{pipe}_i{count}.png",
            self.frames + 1
        );
        eprintln!("inspect: snapshot {name} res {res} {w}x{h}");
        self.end_rendering(d, cb);
        let g = self.gpu;
        // SAFETY: the buffer is created and bound before the copy is recorded; both are freed
        // by `finish_draw_dump` after the submission completed.
        unsafe {
            let Ok(buffer) = d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            ) else {
                return;
            };
            let req = d.get_buffer_memory_requirements(buffer);
            let flags =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let memory = g.memory_type(req.memory_type_bits, flags).and_then(|ty| {
                d.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty),
                    None,
                )
                .ok()
            });
            let Some(memory) = memory else {
                d.destroy_buffer(buffer, None);
                return;
            };
            if d.bind_buffer_memory(buffer, memory, 0).is_err() {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
                return;
            }
            let region = [vk::BufferImageCopy {
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
                ..Default::default()
            }];
            super::replay::global_barrier(d, cb);
            d.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::GENERAL, buffer, &region);
            super::replay::global_barrier(d, cb);
            self.dd.held += size;
            self.dd.snaps.push(Snap {
                name,
                width: w,
                height: h,
                format: info.format,
                buffer,
                memory,
                size,
            });
        }
    }

    /// After the submission: writes the snapshots as PNGs and frees them all.
    pub(super) fn finish_draw_dump(&mut self) {
        let snaps = std::mem::take(&mut self.dd.snaps);
        self.dd.held = 0;
        self.dd.count = 0;
        self.dd.fresh = None;
        if snaps.is_empty() {
            return;
        }
        let d = &self.gpu.device;
        let dir = dir();
        if let Some(dir) = dir {
            let _ = std::fs::create_dir_all(dir);
        }
        for s in snaps {
            // SAFETY: the submission completed; the memory is host visible and `size` long.
            unsafe {
                if let (Some(dir), Ok(p)) = (
                    dir,
                    d.map_memory(s.memory, 0, s.size, vk::MemoryMapFlags::empty()),
                ) {
                    let bytes =
                        std::slice::from_raw_parts(p.cast::<u8>(), s.size as usize).to_vec();
                    d.unmap_memory(s.memory);
                    let rb = super::replay::Readback {
                        width: s.width,
                        height: s.height,
                        format: s.format,
                        bytes,
                    };
                    match rgba8(&rb) {
                        Some(rgba) => {
                            if let Err(e) = write_png(&dir.join(&s.name), s.width, s.height, &rgba)
                            {
                                log_once(format!("cannot write {}: {e}", s.name));
                            }
                        }
                        None => {
                            eprintln!("inspect: {} format {:?} not converted", s.name, s.format)
                        }
                    }
                }
                d.destroy_buffer(s.buffer, None);
                d.free_memory(s.memory, None);
            }
        }
    }

    pub(super) fn inspecting_at_present(&self) -> bool {
        dir().is_some() && super::present::dump_wanted(self.frames)
    }

    /// `NATIV64_DRAW_ONLY=<a>-<b>`: in the frames that are dumped (`NATIV64_DUMP_FRAMES` or
    /// `NATIV64_DUMP_TARGETS`), only the draws numbered `a..=b` in the frame (counted from 0,
    /// replayed ones) are recorded, to see what each part of a frame writes. True when this
    /// draw is left out.
    pub(super) fn skipped_by_filter(&self) -> bool {
        static RANGE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
        static COUNT: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
        let Some((a, b)) = *RANGE.get_or_init(|| {
            let v = std::env::var("NATIV64_DRAW_ONLY").ok()?;
            let (a, b) = v.split_once('-')?;
            let range = (a.parse().ok()?, b.parse().ok()?);
            if std::env::var_os("NATIV64_DUMP_FRAMES").is_none_or(|d| d.is_empty())
                && dir().is_none()
            {
                super::log_once(
                    "NATIV64_DRAW_ONLY has no effect without NATIV64_DUMP_FRAMES or \
                     NATIV64_DUMP_TARGETS: nothing would be rendered to look at"
                        .into(),
                );
                return None;
            }
            Some(range)
        }) else {
            return false;
        };
        if !super::present::dump_wanted(self.frames + 1) {
            return false;
        }
        let mut c = COUNT.lock().unwrap_or_else(|e| e.into_inner());
        if c.0 != self.frames + 1 {
            *c = (self.frames + 1, 0);
        }
        let n = c.1;
        c.1 += 1;
        !(a..=b).contains(&n)
    }

    /// `NATIV64_DRAW_ONLY_PIPELINE=<serial[,serial...]>`: in the frames that are dumped, only the
    /// draws of those pipelines are recorded. Unlike the draw-index filter this names the
    /// pipeline directly, so a batch can be isolated without counting draws. True when this
    /// draw is left out.
    pub(super) fn skipped_by_serial(&self, serial: usize) -> bool {
        static SERIALS: std::sync::OnceLock<Option<Vec<usize>>> = std::sync::OnceLock::new();
        let Some(serials) = SERIALS.get_or_init(|| {
            let v = std::env::var("NATIV64_DRAW_ONLY_PIPELINE").ok()?;
            let mut out = Vec::new();
            for part in v.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                out.push(part.parse().ok()?);
            }
            Some(out)
        }) else {
            return false;
        };
        if !super::present::dump_wanted(self.frames + 1) {
            return false;
        }
        !serials.contains(&serial)
    }

    /// `NATIV64_DRAW_ONLY_PS=<hex hash>`: in the frames that are dumped, only the draws whose
    /// pixel shader is that hash are recorded (which text a pixel shader draws, without knowing
    /// the run's pipeline serial). True when this draw is left out.
    pub(super) fn skipped_by_ps(&self, pd: &crate::d3d12::pipeline::PipelineData) -> bool {
        static HASH: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let Some(hash) = *HASH.get_or_init(|| {
            let v = std::env::var("NATIV64_DRAW_ONLY_PS").ok()?;
            u64::from_str_radix(v.trim(), 16).ok()
        }) else {
            return false;
        };
        if !super::present::dump_wanted(self.frames + 1) {
            return false;
        }
        let ps = pd
            .stages
            .iter()
            .find(|s| s.name == "pixel")
            .map(|s| crate::shader::fnv1a64(&s.bytes));
        ps != Some(hash)
    }

    /// `NATIV64_DRAW_SKIP_PS=<hex hash>[,<hex>]`: in the frames that are dumped, leave out the
    /// draws whose pixel shader is one of those hashes (which layer covers what). True when this
    /// draw is left out.
    pub(super) fn skipped_by_skip_ps(&self, pd: &crate::d3d12::pipeline::PipelineData) -> bool {
        static HASHES: std::sync::OnceLock<Option<Vec<u64>>> = std::sync::OnceLock::new();
        let Some(hashes) = HASHES.get_or_init(|| {
            let v = std::env::var("NATIV64_DRAW_SKIP_PS").ok()?;
            let mut out = Vec::new();
            for part in v.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                out.push(u64::from_str_radix(part, 16).ok()?);
            }
            Some(out)
        }) else {
            return false;
        };
        if !super::present::dump_wanted(self.frames + 1) {
            return false;
        }
        let ps = pd
            .stages
            .iter()
            .find(|s| s.name == "pixel")
            .map(|s| crate::shader::fnv1a64(&s.bytes));
        ps.is_some_and(|h| hashes.contains(&h))
    }

    /// One `inspect:` line per command of the frame, cut at 300 characters.
    pub(super) fn inspect_command(&self, c: &Command) {
        let t = format!("{c:?}");
        let t: String = t.chars().take(300).collect();
        eprintln!("inspect: cmd {} {t}", self.frames + 1);
    }

    /// One `inspect:` line about the draw `c` being recorded.
    pub(super) fn inspect_draw(
        &mut self,
        c: &Command,
        pd: &PipelineData,
        targets: &[TargetRecord],
        extent: (u32, u32),
        rects: bool,
    ) {
        let l = &self.draw.list;
        let fixed = pd
            .graphics
            .as_ref()
            .and_then(|g| fixed_state(g).ok())
            .map(|f| {
                format!(
                    "fill {} cull {} ccw {} depth {:?} blend {:?} mask {:#x}",
                    f.fill,
                    f.cull,
                    f.front_ccw,
                    f.depth,
                    f.targets[0].enable.then_some((
                        f.targets[0].src,
                        f.targets[0].dst,
                        f.targets[0].op,
                        f.targets[0].src_alpha,
                        f.targets[0].dst_alpha,
                        f.targets[0].op_alpha
                    )),
                    f.targets[0].write_mask
                )
            });
        let tg: Vec<String> = targets
            .iter()
            .map(|t| {
                let d = self.image_desc(t.resource);
                format!(
                    "res {} {:?} desc {:?}",
                    t.resource,
                    d.map(|d| (d.width, d.height, d.format)),
                    (t.described != 0).then_some(t.desc)
                )
            })
            .collect();
        eprintln!(
            "inspect: frame {} draw {:?} pipeline {} rects {rects} topology {} targets [{}] extent {extent:?} viewport {:?} scissor {:?} blendconst {:?} fixed {fixed:?}",
            self.frames + 1,
            c,
            pd.serial,
            l.topology,
            tg.join("; "),
            l.viewport,
            l.scissor,
            l.blend,
        );
        if let Some(g) = &pd.graphics {
            let h: String = g.raw.iter().take(520).map(|x| format!("{x:02x}")).collect();
            eprintln!(
                "inspect:   pipeline {} stream {} raw {} bytes: {h}",
                pd.serial,
                g.stream,
                g.raw.len()
            );
        }
        eprintln!("inspect:   root {:?}", l.root);
        eprintln!("inspect:   vertex buffers {:?}", l.vertex);
        let mut addrs: Vec<(String, u64)> = Vec::new();
        for (s, a) in &l.root.tables {
            addrs.push((format!("table {s}"), *a));
        }
        for (s, a) in &l.root.views {
            addrs.push((format!("view {s}"), *a));
        }
        for v in l.vertex.values() {
            addrs.push(("vertex".into(), v.address));
        }
        // The first descriptors of every table, and the memory of the constant buffers.
        for (s, a) in &l.root.tables {
            for k in 0..6u64 {
                let v = crate::d3d12::descriptor::view(a + 32 * k);
                eprintln!("inspect:   table {s} descriptor {k}: {v:?}");
                if let Some(View::Constant(addr, size)) = v {
                    if self
                        .draw
                        .mem
                        .translate(addr, u64::from(size).clamp(1, 128))
                        .is_some()
                    {
                        // SAFETY: translated: committed, readable memory.
                        let h = unsafe { hex(addr, (size as usize).clamp(1, 128)) };
                        eprintln!("inspect:     constant buffer {addr:#x} ({size} bytes): {h}");
                    }
                }
            }
        }
        for (what, a) in addrs {
            if self.draw.mem.translate(a, 128).is_some() {
                // SAFETY: translated: committed, readable memory.
                eprintln!("inspect:   {what} @{a:#x}: {}", unsafe { hex(a, 128) });
            } else {
                eprintln!("inspect:   {what} @{a:#x}: not GPU memory");
            }
        }
    }

    /// Writes every image the replay holds as a PNG (see the module comment).
    pub(super) fn dump_targets(&mut self) {
        let Some(dir) = dir() else { return };
        if let Err(e) = std::fs::create_dir_all(dir) {
            log_once(format!("cannot create {}: {e}", dir.display()));
            return;
        }
        let mut ids: Vec<usize> = self
            .images
            .iter()
            .filter_map(|(&r, i)| i.as_ref().map(|_| r))
            .collect();
        ids.sort_unstable();
        // `NATIV64_INSPECT_BIG=1`: only the full-size targets (the work target, the presented
        // one, the A8 mask) - the whole-image dump is far too slow to reach a UI screen.
        let only_big =
            std::env::var("NATIV64_INSPECT_BIG").is_ok_and(|v| !v.is_empty() && v != "0");
        for res in ids {
            if only_big
                && self
                    .images
                    .get(&res)
                    .and_then(Option::as_ref)
                    .is_none_or(|t| t.desc.width < 1920)
            {
                continue;
            }
            let Some(rb) = self.read_back(res) else {
                continue;
            };
            let owner = self
                .images
                .get(&res)
                .and_then(Option::as_ref)
                .map_or("?", |t| if t.gpu_owned { "gpu" } else { "host" });
            let Some(rgba) = rgba8(&rb) else {
                eprintln!(
                    "inspect: frame {} res {res} {}x{} {:?} ({owner}) not converted",
                    self.frames, rb.width, rb.height, rb.format
                );
                continue;
            };
            let nonzero = rgba.iter().filter(|&&b| b != 0).count();
            eprintln!(
                "inspect: frame {} res {res} {}x{} {:?} ({owner}) nonzero bytes {nonzero}",
                self.frames, rb.width, rb.height, rb.format
            );
            let path = dir.join(format!(
                "frame_{:05}_res{res}_{}x{}.png",
                self.frames, rb.width, rb.height
            ));
            if let Err(e) = write_png(&path, rb.width, rb.height, &rgba) {
                log_once(format!("cannot write {}: {e}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_frame_parsing() {
        assert_eq!(parse_draws_frame(None), None);
        assert_eq!(parse_draws_frame(Some("4800")), Some(4800));
        assert_eq!(parse_draws_frame(Some(" 12 ")), Some(12));
        assert_eq!(parse_draws_frame(Some("0")), None);
        assert_eq!(parse_draws_frame(Some("x")), None);
        assert_eq!(parse_draws_frame(Some("")), None);
    }

    #[test]
    fn snapshot_cap() {
        assert!(snap_fits(0, 0, 33 << 20));
        assert!(!snap_fits(SNAP_MAX, 0, 1));
        assert!(!snap_fits(1, SNAP_BYTES - 10, 11));
        assert!(snap_fits(1, SNAP_BYTES - 10, 10));
    }
}
