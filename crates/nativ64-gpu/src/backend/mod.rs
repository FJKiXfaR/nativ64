//! Execution backends for the recorded D3D12 streams: the Vulkan one, and a null one that
//! only counts (tests, headless runs without a dump directory, `NATIV64_GPU_BACKEND=null`).
//!
//! The backend is chosen at the first submission or present: Vulkan when a device opens and
//! there is something to show it on (the title's window, or `NATIV64_DUMP_FRAMES`), else null.
//! While the only missing piece is a window, the choice is retried at the next call.

mod trace;
pub mod vk;

use crate::d3d12::command::{Command, Rect};
use nativ64::windowing::{main_window, window_client_size, window_surface};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// What the queue and `PresentX` ask of a backend. Every `execute` completes before it
/// returns (the first synchronisation step of docs/gpu.md).
pub trait Backend: Send {
    fn name(&self) -> &'static str;
    /// Replay `lists` in order.
    fn execute(&mut self, lists: &[Vec<Command>]);
    /// Show `resource` (a resource of ours, as a pointer) after the earlier submissions,
    /// restricted to `rect` when given.
    fn present(&mut self, resource: usize, rect: Option<Rect>);
}

/// Counts and completes at once.
#[derive(Default)]
pub struct NullBackend {
    pub lists: u64,
    pub commands: u64,
    pub presents: u64,
}

impl Backend for NullBackend {
    fn name(&self) -> &'static str {
        "null"
    }
    fn execute(&mut self, lists: &[Vec<Command>]) {
        self.lists += lists.len() as u64;
        self.commands += lists.iter().map(|l| l.len() as u64).sum::<u64>();
        trace::execute(lists);
    }
    fn present(&mut self, _: usize, _: Option<Rect>) {
        self.presents += 1;
        trace::present();
    }
}

/// The Vulkan replay on a device that lives for the whole run, presenting to the title's window.
struct VulkanBackend(vk::VkBackend<'static>);

// SAFETY: the replay is only used under the `CURRENT` mutex; its Vulkan handles are plain ids
// and function tables, and the queue is behind its own mutex.
unsafe impl Send for VulkanBackend {}

impl Backend for VulkanBackend {
    fn name(&self) -> &'static str {
        "vulkan"
    }
    fn execute(&mut self, lists: &[Vec<Command>]) {
        trace::execute(lists);
        self.0.execute(lists);
    }
    fn present(&mut self, resource: usize, rect: Option<Rect>) {
        let window = main_window().and_then(|hwnd| {
            let surface = window_surface(hwnd)?;
            let size = window_client_size(hwnd)
                .map_or((0, 0), |(w, h)| (w.max(0) as u32, h.max(0) as u32));
            Some(vk::Window {
                hwnd,
                surface,
                size,
            })
        });
        trace::present();
        self.0.present(resource, rect, window);
    }
}

/// The chosen backend, with whether the choice may still change (null only for lack of a
/// window or a dump directory).
static CURRENT: Mutex<Option<(Box<dyn Backend>, bool)>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn dump_dir() -> Option<PathBuf> {
    std::env::var_os("NATIV64_DUMP_FRAMES")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// The backend for this run, and whether to choose again at the next call.
fn select() -> (Box<dyn Backend>, bool) {
    let null = |retry| (Box::new(NullBackend::default()) as Box<dyn Backend>, retry);
    if std::env::var("NATIV64_GPU_BACKEND").as_deref() == Ok("null") {
        return null(false);
    }
    let dump = dump_dir();
    let has_window = main_window().and_then(window_surface).is_some();
    if !has_window && dump.is_none() {
        return null(true);
    }
    match vk::Gpu::open() {
        Ok(gpu) => {
            eprintln!(
                "nativ64-gpu: Vulkan backend on {} ({})",
                gpu.name(),
                if has_window { "window" } else { "dump only" }
            );
            let gpu: &'static vk::Gpu = Box::leak(Box::new(gpu));
            let mut b = vk::VkBackend::new(gpu);
            if let Some(d) = dump {
                b.set_dump_dir(d);
            }
            (Box::new(VulkanBackend(b)), false)
        }
        Err(e) => {
            eprintln!("nativ64-gpu: no Vulkan ({e}); null backend");
            null(false)
        }
    }
}

/// Run `f` on the run's backend, choosing it first when needed.
pub fn with<R>(f: impl FnOnce(&mut dyn Backend) -> R) -> R {
    let mut cur = lock(&CURRENT);
    if cur.as_ref().is_none_or(|(_, retry)| *retry) {
        *cur = Some(select());
    }
    f(cur.as_mut().unwrap().0.as_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_backend_counts() {
        let mut n = NullBackend::default();
        n.execute(&[vec![Command::Topology(4)], vec![]]);
        n.present(0, None);
        assert_eq!((n.lists, n.commands, n.presents), (2, 1, 1));
    }
}
