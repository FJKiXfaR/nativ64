//! Windowing core: window classes, the window table and per-thread message queues.
//!
//! This is the shared implementation behind the `ext-ms-win-rtcore-ntuser-*` modules (and their
//! `user32` aliases). It implements Win32 semantics only; no real window exists yet.
//!
//! - Classes: process-global registry; name lookup ignores case; atoms start at `0xC000`.
//! - Windows: `HWND` values come from a counter starting at `0x0001_0000` (step 2), a range
//!   disjoint from the kernel handle table. Each window belongs to the thread that created it;
//!   its messages are queued on that thread's queue.
//! - Queues: one per OS thread, keyed by thread id, so any thread may post to another.
//!
//! Host side: the [`WindowBackend`] trait is the seam to the real windowing system. Only the
//! [`HeadlessBackend`] (no window, no events) is the default; a real backend (Wayland, in
//! `nativ64-gpu`) hands out a [`RawSurface`] for Vulkan and feeds resize and close requests
//! back through `poll_events`.
//!
//! Skipped (documented, not forgotten): `WM_GETMINMAXINFO`, `WM_NCCALCSIZE`, `WM_WINDOWPOSCHANGING/
//! CHANGED`, non-client area (client size = window size), z-order, focus/activation chains
//! (`WM_ACTIVATEAPP`, `WM_SETFOCUS`), `WM_PAINT`/`WM_TIMER`, sent (cross-thread) messages, the
//! `W` variants of the functions, window/class extra bytes and menus.

use crate::api_ms_win_core_errorhandling::set_last_error;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INVALID_WINDOW_HANDLE: u32 = 1400;
pub const ERROR_CANNOT_FIND_WND_CLASS: u32 = 1407;
pub const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
pub const ERROR_NOACCESS: u32 = 998;

pub const WM_NULL: u32 = 0x0000;
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_ACTIVATEAPP: u32 = 0x001C;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_SHOWWINDOW: u32 = 0x0018;
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_NCDESTROY: u32 = 0x0082;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_USER: u32 = 0x0400;

pub const SIZE_RESTORED: usize = 0;
pub const SIZE_MINIMIZED: usize = 1;
pub const SIZE_MAXIMIZED: usize = 2;
pub const WA_ACTIVE: usize = 1;

pub const SW_HIDE: i32 = 0;
pub const SW_SHOWMINIMIZED: i32 = 2;
pub const SW_SHOWMAXIMIZED: i32 = 3;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const SW_SHOWMINNOACTIVE: i32 = 7;
pub const SW_SHOWNA: i32 = 8;
pub const SW_MINIMIZE: i32 = 6;

pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const CW_USEDEFAULT: i32 = i32::MIN;

pub const PM_NOREMOVE: u32 = 0;
pub const PM_REMOVE: u32 = 1;

/// The window procedure ABI.
pub type WndProc = unsafe extern "win64" fn(usize, u32, usize, isize) -> isize;

/// `WNDCLASSEXA` (x64 layout, 80 bytes).
#[repr(C)]
pub struct WndClassExA {
    pub cb_size: u32,
    pub style: u32,
    pub wnd_proc: usize,
    pub cb_cls_extra: i32,
    pub cb_wnd_extra: i32,
    pub h_instance: usize,
    pub h_icon: usize,
    pub h_cursor: usize,
    pub hbr_background: usize,
    pub menu_name: *const u8,
    pub class_name: *const u8,
    pub h_icon_sm: usize,
}

/// `CREATESTRUCTA` (x64 layout).
#[repr(C)]
pub struct CreateStructA {
    pub create_params: usize,
    pub h_instance: usize,
    pub h_menu: usize,
    pub hwnd_parent: usize,
    pub cy: i32,
    pub cx: i32,
    pub y: i32,
    pub x: i32,
    pub style: i32,
    pub name: *const u8,
    pub class: *const u8,
    pub ex_style: u32,
}

/// `MSG` (x64 layout, 48 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Msg {
    pub hwnd: usize,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub pt_x: i32,
    pub pt_y: i32,
    pub private: u32,
}

impl Msg {
    pub fn new(hwnd: usize, message: u32, wparam: usize, lparam: isize) -> Msg {
        Msg {
            hwnd,
            message,
            wparam,
            lparam,
            ..Msg::default()
        }
    }
}

/// The host windowing system seen by the Win32 layer.
pub trait WindowBackend: Send {
    /// A window was created (client size `w` x `h`, ANSI title lossily decoded).
    fn on_create(&mut self, hwnd: usize, w: i32, h: i32, title: &str);
    /// Visibility changed.
    fn on_show(&mut self, hwnd: usize, visible: bool);
    /// The window was destroyed.
    fn on_destroy(&mut self, hwnd: usize);
    /// Host events (input, close request, ...) as messages to post; called by every peek.
    fn poll_events(&mut self) -> Vec<Msg>;
    /// The native surface of `hwnd`, for a graphics API to render into (None when the backend
    /// has no real window, as the headless one).
    fn surface(&self, _hwnd: usize) -> Option<RawSurface> {
        None
    }
    /// Current client size of `hwnd` as the host knows it (after resizes by the compositor).
    fn client_size(&self, _hwnd: usize) -> Option<(i32, i32)> {
        None
    }
}

/// A host window surface as raw pointers, so `nativ64` needs no windowing-system crate. The
/// pointers stay valid until `on_destroy` for that window (or the backend is dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawSurface {
    /// `wl_display *` and `wl_surface *` (libwayland objects).
    Wayland {
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    },
}

// SAFETY: the pointers are opaque handles; using them is the receiver's responsibility.
unsafe impl Send for RawSurface {}

/// No window, no events: the only backend for now.
pub struct HeadlessBackend;

impl WindowBackend for HeadlessBackend {
    fn on_create(&mut self, _: usize, _: i32, _: i32, _: &str) {}
    fn on_show(&mut self, _: usize, _: bool) {}
    fn on_destroy(&mut self, _: usize) {}
    fn poll_events(&mut self) -> Vec<Msg> {
        Vec::new()
    }
}

static BACKEND: Mutex<Option<Box<dyn WindowBackend>>> = Mutex::new(None);

/// Installs the host backend (default: headless).
pub fn set_backend(b: Box<dyn WindowBackend>) {
    *lock(&BACKEND) = Some(b);
}

/// The native surface of `hwnd` in the installed backend (None when headless or unknown).
pub fn window_surface(hwnd: usize) -> Option<RawSurface> {
    with_backend(|b| b.surface(hwnd))
}

/// The title's main window: the oldest live top-level window (None before the first).
pub fn main_window() -> Option<usize> {
    windows(|t| {
        t.iter()
            .filter(|(_, w)| w.parent == 0)
            .map(|(&h, _)| h)
            .min()
    })
}

/// The host's current client size of `hwnd` (None when headless or unknown).
pub fn window_client_size(hwnd: usize) -> Option<(i32, i32)> {
    with_backend(|b| b.client_size(hwnd))
}

fn with_backend<R>(f: impl FnOnce(&mut dyn WindowBackend) -> R) -> R {
    let mut g = lock(&BACKEND);
    f(g.get_or_insert_with(|| Box::new(HeadlessBackend)).as_mut())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Class {
    atom: u16,
    name: String,
    wnd_proc: usize,
    #[allow(dead_code)]
    style: u32,
    h_instance: usize,
}

struct Window {
    wnd_proc: usize,
    thread: u32,
    parent: usize,
    visible: bool,
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    x: i32,
    #[allow(dead_code)]
    y: i32,
    w: i32,
    h: i32,
    minimized: bool,
    maximized: bool,
}

static CLASSES: Mutex<Vec<Class>> = Mutex::new(Vec::new());
static NEXT_ATOM: AtomicU16 = AtomicU16::new(0xC000);
static WINDOWS: Mutex<Option<HashMap<usize, Window>>> = Mutex::new(None);
static NEXT_HWND: AtomicUsize = AtomicUsize::new(0x0001_0000);
static QUEUES: Mutex<Option<HashMap<u32, VecDeque<Msg>>>> = Mutex::new(None);

fn windows<R>(f: impl FnOnce(&mut HashMap<usize, Window>) -> R) -> R {
    f(lock(&WINDOWS).get_or_insert_with(HashMap::new))
}

fn queues<R>(f: impl FnOnce(&mut HashMap<u32, VecDeque<Msg>>) -> R) -> R {
    f(lock(&QUEUES).get_or_insert_with(HashMap::new))
}

/// The current OS thread id (owner of windows and queue).
pub fn current_thread() -> u32 {
    thread_local! {
        static TID: u32 = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    }
    TID.with(|t| *t)
}

/// Reads a NUL-terminated ANSI string.
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn read_ansi(p: *const u8) -> String {
    String::from_utf8_lossy(std::ffi::CStr::from_ptr(p.cast()).to_bytes()).into_owned()
}

/// `RegisterClassExA`: returns the atom, or 0 with the last error set.
///
/// # Safety
/// `wc` must be NULL or point to a readable `WNDCLASSEXA` whose names are valid strings.
pub unsafe fn register_class(wc: *const WndClassExA) -> u16 {
    if wc.is_null() {
        set_last_error(ERROR_NOACCESS);
        return 0;
    }
    let wc = &*wc;
    if wc.cb_size as usize != std::mem::size_of::<WndClassExA>()
        || wc.class_name.is_null()
        || (wc.class_name as usize) < 0x1_0000 // atoms as class names are not supported
        || wc.wnd_proc == 0
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let name = read_ansi(wc.class_name);
    if name.is_empty() || name.len() > 256 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut classes = lock(&CLASSES);
    if classes.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
        set_last_error(ERROR_CLASS_ALREADY_EXISTS);
        return 0;
    }
    let atom = NEXT_ATOM.fetch_add(1, Ordering::Relaxed);
    classes.push(Class {
        atom,
        name,
        wnd_proc: wc.wnd_proc,
        style: wc.style,
        h_instance: wc.h_instance,
    });
    atom
}

fn find_class(class: *const u8) -> Option<(usize, usize)> {
    let classes = lock(&CLASSES);
    let c = if (class as usize) < 0x1_0000 {
        classes.iter().find(|c| c.atom as usize == class as usize)
    } else {
        // SAFETY: caller passes a NUL-terminated class name (CreateWindowExA contract).
        let name = unsafe { read_ansi(class) };
        classes.iter().find(|c| c.name.eq_ignore_ascii_case(&name))
    };
    c.map(|c| (c.wnd_proc, c.h_instance))
}

/// Calls the window procedure stored at `proc_addr`.
fn call_proc(proc_addr: usize, hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: the address was supplied through RegisterClassExA as a win64 window procedure.
    unsafe { std::mem::transmute::<usize, WndProc>(proc_addr)(hwnd, msg, w, l) }
}

/// Sends a message synchronously to a window (0 if the window does not exist).
pub fn send_message(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    match windows(|t| t.get(&hwnd).map(|x| x.wnd_proc)) {
        Some(p) => call_proc(p, hwnd, msg, w, l),
        None => 0,
    }
}

/// Appends a message to the queue of the thread owning `hwnd` (or the current thread when
/// `hwnd` is 0). False when the window does not exist.
pub fn post_message(hwnd: usize, msg: u32, w: usize, l: isize) -> bool {
    let tid = if hwnd == 0 {
        current_thread()
    } else {
        match windows(|t| t.get(&hwnd).map(|x| x.thread)) {
            Some(t) => t,
            None => {
                set_last_error(ERROR_INVALID_WINDOW_HANDLE);
                return false;
            }
        }
    };
    queues(|q| {
        q.entry(tid)
            .or_default()
            .push_back(Msg::new(hwnd, msg, w, l))
    });
    true
}

fn makelong(lo: i32, hi: i32) -> isize {
    ((lo as u16 as u32) | ((hi as u16 as u32) << 16)) as isize
}

/// `CreateWindowExA`. Sequence: `WM_NCCREATE`, `WM_CREATE`, then `WM_SIZE` and `WM_MOVE`;
/// `WS_VISIBLE` then shows the window. Failure aborts with `WM_NCDESTROY` (NCCREATE false) or
/// `WM_DESTROY` + `WM_NCDESTROY` (CREATE -1) and returns 0.
///
/// # Safety
/// `class` must be an atom or a valid string; `name` NULL or a valid string.
#[allow(clippy::too_many_arguments)]
pub unsafe fn create_window(
    ex_style: u32,
    class: *const u8,
    name: *const u8,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: usize,
    menu: usize,
    h_instance: usize,
    param: usize,
) -> usize {
    if class.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some((wnd_proc, class_inst)) = find_class(class) else {
        set_last_error(ERROR_CANNOT_FIND_WND_CLASS);
        return 0;
    };
    if parent != 0 && !windows(|t| t.contains_key(&parent)) {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return 0;
    }
    if style & WS_CHILD != 0 && parent == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // CW_USEDEFAULT: fixed defaults instead of cascading (no window manager yet).
    let (x, y) = (
        if x == CW_USEDEFAULT { 0 } else { x },
        if y == CW_USEDEFAULT { 0 } else { y },
    );
    let (w, h) = (
        if w == CW_USEDEFAULT { 640 } else { w },
        if h == CW_USEDEFAULT { 480 } else { h },
    );
    let title = if name.is_null() {
        String::new()
    } else {
        read_ansi(name)
    };
    let hwnd = NEXT_HWND.fetch_add(2, Ordering::Relaxed);
    windows(|t| {
        t.insert(
            hwnd,
            Window {
                wnd_proc,
                thread: current_thread(),
                parent,
                visible: false,
                title: title.clone(),
                x,
                y,
                w,
                h,
                minimized: false,
                maximized: false,
            },
        )
    });
    let cs = CreateStructA {
        create_params: param,
        h_instance: if h_instance != 0 {
            h_instance
        } else {
            class_inst
        },
        h_menu: menu,
        hwnd_parent: parent,
        cy: h,
        cx: w,
        y,
        x,
        // Visibility is applied after WM_CREATE (as WS_VISIBLE is handled by ShowWindow).
        style: (style & !WS_VISIBLE) as i32,
        name,
        class,
        ex_style,
    };
    let csp = &cs as *const CreateStructA as isize;
    if call_proc(wnd_proc, hwnd, WM_NCCREATE, 0, csp) == 0 {
        call_proc(wnd_proc, hwnd, WM_NCDESTROY, 0, 0);
        forget_window(hwnd);
        return 0;
    }
    if call_proc(wnd_proc, hwnd, WM_CREATE, 0, csp) == -1 {
        // The window may already be gone if the procedure destroyed it.
        if windows(|t| t.contains_key(&hwnd)) {
            destroy_window(hwnd);
        }
        return 0;
    }
    if windows(|t| t.contains_key(&hwnd)) {
        with_backend(|b| b.on_create(hwnd, w, h, &title));
        send_message(hwnd, WM_SIZE, SIZE_RESTORED, makelong(w, h));
        send_message(hwnd, WM_MOVE, 0, makelong(x, y));
        if style & WS_VISIBLE != 0 {
            show_window(hwnd, 5);
        }
    }
    hwnd
}

fn forget_window(hwnd: usize) {
    windows(|t| t.remove(&hwnd));
    queues(|q| {
        for m in q.values_mut() {
            m.retain(|x| x.hwnd != hwnd);
        }
    });
}

/// `DestroyWindow` semantics: children first, then `WM_DESTROY` and `WM_NCDESTROY`; queued
/// messages for the window are dropped. (`WM_SHOWWINDOW(0)` for a visible window is skipped.)
pub fn destroy_window(hwnd: usize) -> bool {
    let Some(p) = windows(|t| t.get(&hwnd).map(|x| x.wnd_proc)) else {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return false;
    };
    let children: Vec<usize> = windows(|t| {
        t.iter()
            .filter(|(_, w)| w.parent == hwnd)
            .map(|(h, _)| *h)
            .collect()
    });
    for c in children {
        destroy_window(c);
    }
    call_proc(p, hwnd, WM_DESTROY, 0, 0);
    call_proc(p, hwnd, WM_NCDESTROY, 0, 0);
    forget_window(hwnd);
    with_backend(|b| b.on_destroy(hwnd));
    true
}

/// `ShowWindow`: returns the previous visibility. Hiding sends `WM_SHOWWINDOW(0)`; showing a
/// hidden window sends `WM_SHOWWINDOW(1)`, `WM_SIZE` (restored/minimized/maximized as
/// requested) and, for activating commands on a top-level window, `WM_ACTIVATE(WA_ACTIVE)`.
pub fn show_window(hwnd: usize, cmd: i32) -> bool {
    let Some((prev, top_level)) = windows(|t| t.get(&hwnd).map(|w| (w.visible, w.parent == 0)))
    else {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return false;
    };
    if cmd == SW_HIDE {
        if prev {
            windows(|t| t.get_mut(&hwnd).map(|w| w.visible = false));
            with_backend(|b| b.on_show(hwnd, false));
            send_message(hwnd, WM_SHOWWINDOW, 0, 0);
        }
        return prev;
    }
    let minimize = matches!(cmd, SW_SHOWMINIMIZED | SW_MINIMIZE | SW_SHOWMINNOACTIVE);
    let maximize = cmd == SW_SHOWMAXIMIZED;
    let (state_changed, w, h) = windows(|t| {
        let win = t.get_mut(&hwnd).unwrap();
        let changed = win.minimized != minimize || win.maximized != maximize;
        win.minimized = minimize;
        win.maximized = maximize;
        win.visible = true;
        (changed, win.w, win.h)
    });
    if !prev {
        with_backend(|b| b.on_show(hwnd, true));
        send_message(hwnd, WM_SHOWWINDOW, 1, 0);
    }
    if !prev || state_changed {
        let kind = if minimize {
            SIZE_MINIMIZED
        } else if maximize {
            SIZE_MAXIMIZED
        } else {
            SIZE_RESTORED
        };
        let lp = if minimize { 0 } else { makelong(w, h) };
        send_message(hwnd, WM_SIZE, kind, lp);
    }
    let activates = !matches!(cmd, SW_SHOWNOACTIVATE | SW_SHOWMINNOACTIVE | SW_SHOWNA);
    if !prev && top_level && activates {
        // Windows sends the app-level activation first, then the window's, then the focus; a
        // title that watches WM_ACTIVATEAPP for its intro videos or menu input needs all three.
        // SAFETY: the thread id query takes no pointers.
        let thread = unsafe { crate::api_ms_win_core_processthreads::GetCurrentThreadId() };
        send_message(hwnd, WM_ACTIVATEAPP, 1, thread as isize);
        send_message(hwnd, WM_ACTIVATE, WA_ACTIVE, 0);
        send_message(hwnd, WM_SETFOCUS, 0, 0);
    }
    prev
}

/// `PeekMessageA` for the calling thread. `hwnd` 0 matches every message of the thread,
/// `usize::MAX` (-1) only thread messages (null hwnd); a range of 0..0 matches everything.
/// Never blocks. Host events are pulled from the backend first.
///
/// # Safety
/// `out` must be NULL or point to a writable `MSG`.
pub unsafe fn peek_message(out: *mut Msg, hwnd: usize, min: u32, max: u32, flags: u32) -> bool {
    if out.is_null() {
        set_last_error(ERROR_NOACCESS);
        return false;
    }
    if hwnd != 0 && hwnd != usize::MAX && !windows(|t| t.contains_key(&hwnd)) {
        set_last_error(ERROR_INVALID_WINDOW_HANDLE);
        return false;
    }
    for m in with_backend(|b| b.poll_events()) {
        // Events for unknown windows are dropped (the window is gone).
        if m.hwnd == 0 || windows(|t| t.contains_key(&m.hwnd)) {
            post_message(m.hwnd, m.message, m.wparam, m.lparam);
        }
    }
    let any_range = min == 0 && max == 0;
    let found = queues(|q| {
        let queue = q.get_mut(&current_thread())?;
        let i = queue.iter().position(|m| {
            let h_ok = match hwnd {
                0 => true,
                usize::MAX => m.hwnd == 0,
                h => m.hwnd == h,
            };
            h_ok && (any_range || (m.message >= min && m.message <= max))
        })?;
        if flags & PM_REMOVE != 0 {
            queue.remove(i)
        } else {
            queue.get(i).copied()
        }
    });
    match found {
        Some(m) => {
            *out = m;
            true
        }
        None => false,
    }
}

/// `DispatchMessageA`: calls the target window procedure; 0 for a null or unknown window.
///
/// # Safety
/// `msg` must be NULL or point to a readable `MSG`.
pub unsafe fn dispatch_message(msg: *const Msg) -> isize {
    if msg.is_null() {
        set_last_error(ERROR_NOACCESS);
        return 0;
    }
    let m = *msg;
    if m.hwnd == 0 {
        return 0;
    }
    match windows(|t| t.get(&m.hwnd).map(|x| x.wnd_proc)) {
        // WM_TIMER callbacks (lParam) and WM_PAINT validation are not handled.
        Some(p) => call_proc(p, m.hwnd, m.message, m.wparam, m.lparam),
        None => {
            set_last_error(ERROR_INVALID_WINDOW_HANDLE);
            0
        }
    }
}

/// `TranslateMessage`: no `WM_CHAR` generation yet (TODO with the input backend), so it
/// reports FALSE for every message, as Windows does for non-key messages.
///
/// # Safety
/// `msg` must be NULL or point to a readable `MSG`.
pub unsafe fn translate_message(msg: *const Msg) -> bool {
    if msg.is_null() {
        set_last_error(ERROR_NOACCESS);
    }
    false
}

/// `DefWindowProcA`: `WM_NCCREATE` -> TRUE, `WM_CLOSE` -> `DestroyWindow`, anything else 0.
pub fn def_window_proc(hwnd: usize, msg: u32, _w: usize, _l: isize) -> isize {
    match msg {
        WM_NCCREATE => 1,
        WM_CLOSE => {
            destroy_window(hwnd);
            0
        }
        _ => 0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static LOG: RefCell<Vec<(u32, usize, isize)>> = const { RefCell::new(Vec::new()) };
        static NCCREATE_RET: std::cell::Cell<isize> = const { std::cell::Cell::new(1) };
        static CREATE_RET: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    }

    pub(crate) unsafe extern "win64" fn proc_(h: usize, m: u32, w: usize, l: isize) -> isize {
        LOG.with(|v| v.borrow_mut().push((m, w, l)));
        match m {
            WM_NCCREATE => NCCREATE_RET.with(|c| c.get()),
            WM_CREATE => CREATE_RET.with(|c| c.get()),
            WM_USER => 1234,
            _ => def_window_proc(h, m, w, l),
        }
    }

    fn take_log() -> Vec<u32> {
        LOG.with(|v| v.borrow_mut().drain(..).map(|e| e.0).collect())
    }

    pub(crate) fn class(name: &std::ffi::CStr) -> u16 {
        let wc = WndClassExA {
            cb_size: std::mem::size_of::<WndClassExA>() as u32,
            style: 0,
            wnd_proc: proc_ as *const () as usize,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: 0,
            h_icon: 0,
            h_cursor: 0,
            hbr_background: 0,
            menu_name: std::ptr::null(),
            class_name: name.as_ptr().cast(),
            h_icon_sm: 0,
        };
        unsafe { register_class(&wc) }
    }

    fn make(name: &std::ffi::CStr, style: u32) -> usize {
        unsafe {
            create_window(
                0,
                name.as_ptr().cast(),
                c"title".as_ptr().cast(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                320,
                200,
                0,
                0,
                0,
                0,
            )
        }
    }

    #[test]
    fn headless_has_no_surface() {
        assert_eq!(HeadlessBackend.surface(0x10000), None);
        // The trait default is what other backends without a window inherit.
        struct Bare;
        impl WindowBackend for Bare {
            fn on_create(&mut self, _: usize, _: i32, _: i32, _: &str) {}
            fn on_show(&mut self, _: usize, _: bool) {}
            fn on_destroy(&mut self, _: usize) {}
            fn poll_events(&mut self) -> Vec<Msg> {
                Vec::new()
            }
        }
        assert_eq!(Bare.surface(1), None);
    }

    #[test]
    fn layouts() {
        assert_eq!(std::mem::size_of::<WndClassExA>(), 80);
        assert_eq!(std::mem::size_of::<Msg>(), 48);
        assert_eq!(std::mem::size_of::<CreateStructA>(), 80);
    }

    #[test]
    fn register_errors() {
        let name = c"w_reg";
        assert!(class(name) >= 0xC000);
        assert_eq!(class(c"W_REG"), 0, "case-insensitive duplicate");
        assert_eq!(
            unsafe { crate::api_ms_win_core_errorhandling::GetLastError() },
            ERROR_CLASS_ALREADY_EXISTS
        );
        let mut wc = WndClassExA {
            cb_size: 79,
            style: 0,
            wnd_proc: proc_ as *const () as usize,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: 0,
            h_icon: 0,
            h_cursor: 0,
            hbr_background: 0,
            menu_name: std::ptr::null(),
            class_name: c"w_reg2".as_ptr().cast(),
            h_icon_sm: 0,
        };
        unsafe {
            assert_eq!(register_class(&wc), 0);
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_INVALID_PARAMETER
            );
            wc.cb_size = 80;
            wc.class_name = std::ptr::null();
            assert_eq!(register_class(&wc), 0);
            assert_eq!(
                crate::api_ms_win_core_errorhandling::GetLastError(),
                ERROR_INVALID_PARAMETER
            );
        }
    }

    #[test]
    fn creation_sequence_and_show() {
        class(c"w_seq");
        let h = make(c"w_seq", 0);
        assert!(h >= 0x10000);
        assert_eq!(
            take_log(),
            [WM_NCCREATE, WM_CREATE, WM_SIZE, WM_MOVE],
            "hidden window"
        );
        assert!(!show_window(h, 5));
        // the activation triplet: app, window, focus
        assert_eq!(
            take_log(),
            [
                WM_SHOWWINDOW,
                WM_SIZE,
                WM_ACTIVATEAPP,
                WM_ACTIVATE,
                WM_SETFOCUS
            ]
        );
        assert!(show_window(h, 5), "previous visibility");
        assert!(take_log().is_empty());
        assert!(show_window(h, SW_HIDE));
        assert_eq!(take_log(), [WM_SHOWWINDOW]);
        assert!(!show_window(h, SW_HIDE));
        assert!(!show_window(0xdead, 5));
        assert_eq!(
            unsafe { crate::api_ms_win_core_errorhandling::GetLastError() },
            ERROR_INVALID_WINDOW_HANDLE
        );
    }

    #[test]
    fn create_params_and_visible_style() {
        class(c"w_vis");
        let h = make(c"w_vis", WS_VISIBLE);
        assert_eq!(
            take_log(),
            [
                WM_NCCREATE,
                WM_CREATE,
                WM_SIZE,
                WM_MOVE,
                WM_SHOWWINDOW,
                WM_SIZE,
                WM_ACTIVATEAPP,
                WM_ACTIVATE,
                WM_SETFOCUS
            ]
        );
        assert!(show_window(h, 5));
    }

    #[test]
    fn unknown_class() {
        assert_eq!(make(c"w_nope", 0), 0);
        assert_eq!(
            unsafe { crate::api_ms_win_core_errorhandling::GetLastError() },
            ERROR_CANNOT_FIND_WND_CLASS
        );
    }

    #[test]
    fn nccreate_false_aborts() {
        class(c"w_nc");
        NCCREATE_RET.with(|c| c.set(0));
        let h = make(c"w_nc", 0);
        NCCREATE_RET.with(|c| c.set(1));
        assert_eq!(h, 0);
        assert_eq!(take_log(), [WM_NCCREATE, WM_NCDESTROY]);
    }

    #[test]
    fn create_minus_one_aborts() {
        class(c"w_cr");
        CREATE_RET.with(|c| c.set(-1));
        let h = make(c"w_cr", 0);
        CREATE_RET.with(|c| c.set(0));
        assert_eq!(h, 0);
        assert_eq!(
            take_log(),
            [WM_NCCREATE, WM_CREATE, WM_DESTROY, WM_NCDESTROY]
        );
    }

    #[test]
    fn peek_filters_and_dispatch() {
        class(c"w_q");
        let a = make(c"w_q", 0);
        let b = make(c"w_q", 0);
        take_log();
        let mut m = Msg::default();
        unsafe {
            assert!(!peek_message(&mut m, 0, 0, 0, PM_REMOVE), "empty");
            assert!(post_message(a, WM_USER, 1, 2));
            assert!(post_message(b, WM_USER + 1, 3, 4));
            assert!(post_message(0, WM_USER + 2, 5, 6));
            // Range filter skips the first message.
            assert!(peek_message(
                &mut m,
                0,
                WM_USER + 1,
                WM_USER + 1,
                PM_NOREMOVE
            ));
            assert_eq!((m.hwnd, m.wparam, m.lparam), (b, 3, 4));
            // Window filter.
            assert!(peek_message(&mut m, b, 0, 0, PM_NOREMOVE));
            assert_eq!(m.message, WM_USER + 1);
            // Thread-message filter.
            assert!(peek_message(&mut m, usize::MAX, 0, 0, PM_REMOVE));
            assert_eq!((m.hwnd, m.message), (0, WM_USER + 2));
            // NOREMOVE kept the others; removal in order.
            assert!(peek_message(&mut m, 0, 0, 0, PM_REMOVE));
            assert_eq!((m.hwnd, m.message), (a, WM_USER));
            assert_eq!(dispatch_message(&m), 1234);
            assert!(peek_message(&mut m, 0, 0, 0, PM_REMOVE));
            assert_eq!(m.hwnd, b);
            assert!(!peek_message(&mut m, 0, 0, 0, PM_REMOVE));
            // Errors.
            assert!(!peek_message(&mut m, 0xbeef, 0, 0, PM_REMOVE));
            assert!(!peek_message(std::ptr::null_mut(), 0, 0, 0, PM_REMOVE));
            assert_eq!(dispatch_message(&Msg::new(0, WM_USER, 0, 0)), 0);
            assert_eq!(dispatch_message(&Msg::new(0x7777, WM_USER, 0, 0)), 0);
            assert!(!translate_message(&Msg::new(a, WM_KEYDOWN, 65, 0)));
        }
        assert!(!post_message(0x7777, WM_USER, 0, 0));
    }

    #[test]
    fn close_destroys_and_drops_queue() {
        class(c"w_cl");
        let h = make(c"w_cl", 0);
        take_log();
        post_message(h, WM_USER, 0, 0);
        assert_eq!(send_message(h, WM_CLOSE, 0, 0), 0);
        assert_eq!(take_log(), [WM_CLOSE, WM_DESTROY, WM_NCDESTROY]);
        let mut m = Msg::default();
        assert!(!unsafe { peek_message(&mut m, 0, 0, 0, PM_REMOVE) });
        assert!(!destroy_window(h));
    }

    #[test]
    fn queues_are_per_thread() {
        class(c"w_thr");
        let h = make(c"w_thr", 0);
        take_log();
        post_message(h, WM_USER, 0, 0);
        std::thread::spawn(|| {
            let mut m = Msg::default();
            assert!(!unsafe { peek_message(&mut m, 0, 0, 0, PM_REMOVE) });
        })
        .join()
        .unwrap();
        let mut m = Msg::default();
        assert!(unsafe { peek_message(&mut m, 0, 0, 0, PM_REMOVE) });
    }
}
