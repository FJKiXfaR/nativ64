//! `NATIV64_DUMP_TARGETS=<dir>`: with it, the frames the dump keeps (`present::dump_wanted`)
//! also get one `inspect:` line on stderr per replayed draw (pipeline, targets, viewport,
//! scissor, fixed state, root arguments and the first bytes of the memory they point to) and,
//! after the present, every image the replay holds as `<dir>/frame_NNNNN_res<id>_<w>x<h>.png`
//! (render targets and textures alike; formats the PNG dump does not convert are named in the
//! log instead). A debugging aid: it reads back every image, so it is slow.

use super::log_once;
use super::present::{rgba8, write_png};
use super::replay::VkBackend;
use crate::d3d12::command::Command;
use crate::d3d12::descriptor::{TargetRecord, View};
use crate::d3d12::pipeline::PipelineData;
use crate::d3d12::pipeline_state::fixed_state;
use std::path::PathBuf;
use std::sync::OnceLock;

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
        dir().is_some() && super::present::dump_wanted(self.frames + 1)
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
        for res in ids {
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
