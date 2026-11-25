//! Threads created by nativ64 (`CreateThread`, `_beginthreadex`): the hooks that give them a
//! TEB, static TLS and the `DLL_THREAD_ATTACH/DETACH` notifications of the loaded images.
//!
//! `Image::run_main` calls [`install`] with the images it ran; nativ64 then calls [`attach`] on
//! each new thread before its start routine and [`detach`] after it (also after
//! `_endthreadex`).

use crate::teb::{self, Teb};
use crate::tls::{self, ThreadTls, TlsInfo};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

/// A loaded image that gets thread notifications: a package DLL, or the main image last.
pub(crate) struct Module {
    pub base: u64,
    /// `DllMain` of a DLL; `None` for the exe and for a DLL without entry point.
    pub dll_main: Option<u64>,
    pub tls: Option<TlsInfo>,
}

/// Every image's static TLS (index, directory) and the modules in load order.
pub(crate) struct ThreadImages {
    pub tls_images: Vec<(usize, TlsInfo)>,
    pub modules: Vec<Module>,
}

static IMAGES: Mutex<Option<Arc<ThreadImages>>> = Mutex::new(None);

/// Makes `images` the set new threads attach to, and registers the hooks with nativ64.
pub(crate) fn install(images: ThreadImages) {
    *IMAGES.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(images));
    nativ64::process::set_thread_hooks(attach, detach);
}

struct Guard {
    images: Arc<ThreadImages>,
    // Dropped in this order: the TLS blocks, then the TEB.
    _tls: Option<ThreadTls>,
    _teb: Teb,
}

fn notify(m: &Module, reason: u32) {
    if let Some(i) = &m.tls {
        // SAFETY: the TEB and TLS of this thread are installed; the callbacks are guest code.
        unsafe { tls::run_callbacks_reason(i, m.base, reason) };
    }
    if let Some(main) = m.dll_main {
        if !nativ64::process::thread_calls_disabled(m.base as usize) {
            // SAFETY: the entry point of a mapped DLL is a DllMain.
            let f: extern "win64" fn(u64, u32, usize) -> i32 =
                unsafe { std::mem::transmute(main as usize) };
            f(m.base, reason, 0);
        }
    }
}

/// Runs on a new thread: TEB, TLS, then each image's thread-attach notification in load order.
/// Returns an opaque guard for [`detach`], or null when nothing could be set up.
pub(crate) fn attach() -> *mut c_void {
    let Some(images) = IMAGES.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return std::ptr::null_mut();
    };
    crate::fault::setup_thread();
    let Ok((lo, hi)) = crate::image::current_stack() else {
        return std::ptr::null_mut();
    };
    let Ok(teb) = teb::install_for_current_thread(hi, lo) else {
        return std::ptr::null_mut();
    };
    let mut block =
        (!images.tls_images.is_empty()).then(|| unsafe { ThreadTls::new(&images.tls_images) });
    if let Some(b) = block.as_mut() {
        teb.set_tls_pointer(b.array());
    }
    for m in &images.modules {
        notify(m, DLL_THREAD_ATTACH);
    }
    Box::into_raw(Box::new(Guard {
        images,
        _tls: block,
        _teb: teb,
    }))
    .cast()
}

/// Runs on the thread after its start routine: thread-detach notifications in reverse order,
/// then the TLS blocks and the TEB are freed. A null guard does nothing.
pub(crate) fn detach(guard: *mut c_void) {
    if guard.is_null() {
        return;
    }
    // SAFETY: `guard` comes from `attach` on this thread and is released once.
    let g = unsafe { Box::from_raw(guard.cast::<Guard>()) };
    for m in g.images.modules.iter().rev() {
        notify(m, DLL_THREAD_DETACH);
    }
    drop(g);
}
