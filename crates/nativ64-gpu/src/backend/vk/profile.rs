//! Optional timing of the replay (`NATIV64_PROFILE=1`): the time spent in each phase of the
//! submissions and a few counters, printed to stderr every 60 presented frames, then reset.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum Phase {
    /// The texture sync passes of the submissions (everything below is part of it).
    Sync,
    /// Write-watch registration and scans.
    Watch,
    /// Hashing of texture memory.
    Hash,
    /// Detiling and staging of the uploads.
    Upload,
    /// Recording and executing the lists (the wait for the GPU included).
    Record,
    /// Frame dumps (PNG).
    Dump,
    /// Making the images of the textures.
    Image,
    /// Writing the descriptors.
    Descr,
    /// The recording of the sync passes (the rest of `Sync` is the submission and its wait).
    SyncRec,
    /// `host_upload` and `placed_upload` as a whole.
    HostUp,
    PlacedUp,
    /// `GpuMemory::sync` and the view invalidation before a submission.
    Begin,
    /// The view table update (records written, memory imported for them).
    Views,
    /// `import_host` calls.
    Import,
    /// The commands of the lists, recorded (the submission and its wait excluded).
    Cmds,
    /// The compute dispatches among them.
    Disp,
}

#[derive(Clone, Copy)]
pub(super) enum Count {
    /// Textures looked at by the sync, and those the watch let skip.
    Looked,
    Skipped,
    /// Scans that could not tell (taken as written), and scan system calls.
    ScanNone,
    ScanCalls,
    /// Times the write log was dropped, and its largest length.
    LogDrops,
    LogMax,
    /// Bytes hashed, textures uploaded.
    Hashed,
    Uploads,
    /// Submissions.
    Submissions,
    /// Entries of the source lists visited, and descriptors written.
    Visited,
    Written,
    /// Import ranges dropped, view records rewritten, imports made.
    Dropped,
    Rewritten,
    Imports,
}

const PHASES: [&str; 16] = [
    "sync", "watch", "hash", "upload", "record", "dump", "image", "descr", "syncrec", "hostup",
    "placedup", "begin", "views", "import", "cmds", "disp",
];
const COUNTS: [&str; 14] = [
    "looked",
    "skipped",
    "scan_none",
    "scan_calls",
    "log_drops",
    "log_max",
    "hashed_mb",
    "uploads",
    "submissions",
    "visited",
    "descr_written",
    "dropped",
    "rewritten",
    "imports",
];

static NANOS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
static COUNT: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NATIV64_PROFILE").is_some_and(|v| !v.is_empty()))
}

/// The start of a timed phase: None when profiling is off.
pub(super) fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Adds the time since `since` to `phase`.
pub(super) fn stop(phase: Phase, since: Option<Instant>) {
    if let Some(t) = since {
        NANOS[phase as usize].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    }
}

pub(super) fn count(c: Count, n: u64) {
    if enabled() {
        COUNT[c as usize].fetch_add(n, Relaxed);
    }
}

pub(super) fn count_max(c: Count, n: u64) {
    if enabled() {
        COUNT[c as usize].fetch_max(n, Relaxed);
    }
}

/// Prints and resets the totals of the last `frames` frames.
pub(super) fn report(frame: u64, frames: u64) {
    if !enabled() {
        return;
    }
    let mut line = format!("nativ64-profile: frame {frame} (last {frames}):");
    for (name, n) in PHASES.iter().zip(&NANOS) {
        line += &format!(" {name}={:.2}s", n.swap(0, Relaxed) as f64 / 1e9);
    }
    for (i, (name, n)) in COUNTS.iter().zip(&COUNT).enumerate() {
        let v = n.swap(0, Relaxed);
        line += &format!(
            " {name}={}",
            if i == Count::Hashed as usize {
                v >> 20
            } else {
                v
            }
        );
    }
    eprintln!("{line}");
}
