//! `NATIV64_TRACE_DRAWS=1`: one line per presented frame (what the title drew), and the
//! per-pipeline first-frame / use counts of `pipelines.txt`. Shared by every backend, so a
//! headless run on the null backend is traced too.

use std::collections::BTreeSet;

use crate::d3d12::command::Command;

/// What one frame (the commands between two presents) did.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct FrameTrace {
    pub draw: u32,
    pub draw_indexed: u32,
    pub dispatch_mesh: u32,
    pub dispatch: u32,
    pub indirect: u32,
    pub clears: u32,
    pub copies: u32,
    /// What the replay did with the draws (`drawn=N skips=...`), when a backend reports it.
    pub replay: Option<String>,
    /// Ids (creation order) of the pipelines the draws and dispatches used; `None` is a
    /// pipeline object this process did not create.
    pub pipelines: BTreeSet<Option<usize>>,
    current: usize,
}

/// Whether frame `n` (1-based) gets a line: 1..=120, then every 60th.
pub fn wanted(n: u64) -> bool {
    n <= 120 || n.is_multiple_of(60)
}

/// True when `NATIV64_TRACE_DRAWS` is set to something other than empty or 0.
pub fn enabled() -> bool {
    std::env::var_os("NATIV64_TRACE_DRAWS").is_some_and(|v| !v.is_empty() && v != "0")
}

impl FrameTrace {
    /// A new command list starts: no pipeline bound until it binds one.
    pub fn new_list(&mut self) {
        self.current = 0;
    }

    /// Counts `c`; `frame` is the 1-based index of the frame it belongs to.
    pub fn note(&mut self, c: &Command, frame: u64) {
        use Command as C;
        match c {
            C::SetPipeline { pipeline } | C::ClearState { pipeline } => self.current = *pipeline,
            C::Draw { .. } => self.draw += 1,
            C::DrawIndexed { .. } => self.draw_indexed += 1,
            C::DispatchMesh { .. } | C::DispatchMeshX { .. } => self.dispatch_mesh += 1,
            C::Dispatch { .. } => self.dispatch += 1,
            C::ExecuteIndirect { .. } => self.indirect += 1,
            C::ClearRenderTarget { .. } | C::ClearDepthStencil { .. } | C::ClearUav { .. } => {
                self.clears += 1
            }
            C::CopyBufferRegion { .. }
            | C::CopyTextureRegion { .. }
            | C::CopyResource { .. }
            | C::CopyMemory { .. } => self.copies += 1,
            _ => {}
        }
        if matches!(
            c,
            C::Draw { .. }
                | C::DrawIndexed { .. }
                | C::DispatchMesh { .. }
                | C::DispatchMeshX { .. }
                | C::Dispatch { .. }
                | C::ExecuteIndirect { .. }
        ) {
            self.pipelines
                .insert(crate::shader::note_pipeline_use(self.current, frame));
        }
    }

    /// The compact line for frame `n`.
    pub fn line(&self, n: u64) -> String {
        let ids: Vec<String> = self
            .pipelines
            .iter()
            .map(|p| p.map_or("?".to_string(), |p| p.to_string()))
            .collect();
        let replay = self.replay.as_deref().map_or(String::new(), |r| format!(" {r}"));
        format!(
            "frame {n}: draw={} idx={} mesh={} disp={} ind={} clear={} copy={} pipes=[{}]{replay}",
            self.draw,
            self.draw_indexed,
            self.dispatch_mesh,
            self.dispatch,
            self.indirect,
            self.clears,
            self.copies,
            ids.join(",")
        )
    }
}

/// The run's frame counter and the trace of the frame in progress. Global, because a backend
/// still waiting for a window is recreated at every call.
#[derive(Default)]
struct Tracer {
    frame: FrameTrace,
    frames: u64,
}

static TRACER: std::sync::Mutex<Option<Tracer>> = std::sync::Mutex::new(None);

fn with_tracer(f: impl FnOnce(&mut Tracer)) {
    let mut g = TRACER.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Tracer::default));
}

/// Counts the commands of `lists` into the current frame.
pub fn execute(lists: &[Vec<Command>]) {
    with_tracer(|t| t.execute(lists));
}

/// A frame was presented: prints its line when wanted, flushes `pipelines.txt`. `replay` is
/// appended to the line (what the backend did with the frame's draws).
pub fn present(replay: Option<String>) {
    with_tracer(|t| {
        t.frame.replay = replay;
        t.present();
    });
}

impl Tracer {
    fn execute(&mut self, lists: &[Vec<Command>]) {
        for list in lists {
            self.frame.new_list();
            for c in list {
                self.frame.note(c, self.frames + 1);
            }
        }
    }

    fn present(&mut self) {
        self.frames += 1;
        let frame = std::mem::take(&mut self.frame);
        if enabled() && wanted(self.frames) {
            eprintln!("nativ64: {}", frame.line(self.frames));
        }
        crate::shader::flush_pipelines(wanted(self.frames));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rule() {
        assert!(wanted(1) && wanted(120) && !wanted(121) && wanted(180) && !wanted(181));
    }

    #[test]
    fn counts_kinds_and_pipelines() {
        let mut t = FrameTrace::default();
        t.new_list();
        t.note(&Command::SetPipeline { pipeline: 0x40 }, 1);
        t.note(
            &Command::Draw {
                vertices: 3,
                instances: 1,
                first_vertex: 0,
                first_instance: 0,
            },
            1,
        );
        t.note(
            &Command::Dispatch {
                x: 1,
                y: 1,
                z: 1,
                flags: 0,
            },
            1,
        );
        t.note(
            &Command::CopyResource {
                dst: 1,
                src: 2,
                flags: 0,
            },
            1,
        );
        assert_eq!((t.draw, t.dispatch, t.copies), (1, 1, 1));
        assert_eq!(
            t.line(1),
            "frame 1: draw=1 idx=0 mesh=0 disp=1 ind=0 clear=0 copy=1 pipes=[?]"
        );
        t.replay = Some("drawn=1 skips=0".into());
        assert!(t.line(1).ends_with("pipes=[?] drawn=1 skips=0"));
    }
}
