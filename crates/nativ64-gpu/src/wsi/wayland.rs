//! `WindowBackend` over Wayland (`wayland-client` with `system`, so the raw `wl_display *` and
//! `wl_surface *` exist for Vulkan; `libwayland-client` is opened at run time).
//!
//! Thread model: the backend sits in the global `Mutex` of `nativ64::windowing`, and every hook
//! (`on_create`, `on_show`, `on_destroy`, `poll_events`) runs on the title thread that owns the
//! window, so the main event queue is only ever read from one thread at a time. The connection
//! is made in `new()` (to fail early without a compositor); globals are bound and the queue is
//! created on the first `on_create`. Nothing here blocks except that one roundtrip.
//!
//! The one exception is `xdg_wm_base`, which lives on its own event queue served by a small
//! helper thread (`ping_loop`): the compositor's `ping` must be answered even while the title
//! does not poll for a long time (loading), or the window is flagged "not responding". The
//! helper reads the shared socket with the usual `prepare_read` / `poll` (100 ms timeout, so it
//! sees the stop flag) and dispatches only its own queue; everything created from `xdg_wm_base`
//! (`xdg_surface`, `xdg_toplevel`) is still put on the main queue. Events it reads for the main
//! queue just wait there for the next `poll_events`. The thread is joined when the backend drops.
//!
//! One `xdg_toplevel` per HWND. We never attach a buffer ourselves: the window is mapped by the
//! first buffer the Vulkan swapchain presents, and `on_show(false)` unmaps it by attaching a
//! null buffer (the next present, after an initial commit and configure, maps it again).
//! The compositor's size hint (`xdg_toplevel.configure`) becomes `WM_SIZE`; the swapchain
//! picks its extent from `client_size`. Server-side decorations are requested when
//! `xdg-decoration` is offered, otherwise the window has none. Focus comes from the
//! `activated` toplevel state (`WM_ACTIVATE`).
//!
//! Keyboard: `wl_seat` (version <= 5) is bound on the main queue, so its events arrive in
//! `poll_events`; a `wl_keyboard` is held while the seat has the capability. The keymap fd is
//! closed unread: `wl_keyboard.key` codes are raw evdev codes, mapped by `key_button` to
//! gamepad bits (Enter / Space = A; Escape / Backspace = B; X; Y; arrows = dpad; Tab = View;
//! M = Menu; Q / E = shoulders; positions of a QWERTY layout). The pressed set is folded to a
//! mask and handed to `gameinput::set_keyboard_buttons` on each change; key repeat does not
//! change the set, and `leave` releases everything. No `WM_KEY*` messages are generated.

use nativ64::gameinput;
use nativ64::windowing::{
    Msg, RawSurface, WindowBackend, SIZE_MAXIMIZED, SIZE_RESTORED, WA_ACTIVE, WM_ACTIVATE,
    WM_CLOSE, WM_SIZE,
};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_surface};
use wayland_client::WEnum;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

#[derive(Debug)]
pub enum WaylandError {
    /// No compositor reachable (`WAYLAND_DISPLAY` unset, socket missing, no libwayland...).
    Connect(String),
}

impl std::fmt::Display for WaylandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaylandError::Connect(e) => write!(f, "cannot connect to a Wayland compositor: {e}"),
        }
    }
}

impl std::error::Error for WaylandError {}

// ---- Pure message mapping (unit-tested without a compositor) ----

fn makelong(lo: i32, hi: i32) -> isize {
    ((lo as u16 as u32) | ((hi as u16 as u32) << 16)) as isize
}

/// Toplevel states of an `xdg_toplevel.configure`: (activated, maximized).
fn decode_states(states: &[u8]) -> (bool, bool) {
    let mut out = (false, false);
    for c in states.as_chunks::<4>().0 {
        match u32::from_ne_bytes(*c) {
            1 => out.1 = true, // maximized
            4 => out.0 = true, // activated
            _ => {}
        }
    }
    out
}

/// The `WM_SIZE` for a compositor size hint, or None when it changes nothing. A hint of 0 on
/// an axis means "your choice": that axis keeps its current value.
fn size_msg(
    hwnd: usize,
    cur: (i32, i32),
    hint: (i32, i32),
    maximized: bool,
) -> Option<(Msg, (i32, i32))> {
    let new = (
        if hint.0 > 0 { hint.0 } else { cur.0 },
        if hint.1 > 0 { hint.1 } else { cur.1 },
    );
    if new == cur {
        return None;
    }
    let kind = if maximized {
        SIZE_MAXIMIZED
    } else {
        SIZE_RESTORED
    };
    Some((Msg::new(hwnd, WM_SIZE, kind, makelong(new.0, new.1)), new))
}

/// `WM_ACTIVATE` on a focus change (`WA_INACTIVE` is 0).
fn activate_msg(hwnd: usize, was: bool, now: bool) -> Option<Msg> {
    (was != now).then(|| Msg::new(hwnd, WM_ACTIVATE, if now { WA_ACTIVE } else { 0 }, 0))
}

/// The gamepad bit of a Linux evdev key code (0 when the key does nothing).
fn key_button(code: u32) -> u32 {
    match code {
        28 | 57 => gameinput::A, // Enter, Space
        1 | 14 => gameinput::B,  // Escape, Backspace
        45 => gameinput::X,      // X
        21 => gameinput::Y,      // Y
        103 => gameinput::UP,
        108 => gameinput::DOWN,
        105 => gameinput::LEFT,
        106 => gameinput::RIGHT,
        15 => gameinput::VIEW,           // Tab
        50 => gameinput::MENU,           // M
        16 => gameinput::LEFT_SHOULDER,  // Q
        18 => gameinput::RIGHT_SHOULDER, // E
        _ => 0,
    }
}

/// The buttons held for a set of pressed evdev codes.
fn keys_mask(pressed: &[u32]) -> u32 {
    pressed.iter().fold(0, |m, &c| m | key_button(c))
}

/// Press or release `code` in the set; true when the set changed (repeats change nothing).
fn apply_key(pressed: &mut Vec<u32>, code: u32, down: bool) -> bool {
    match (pressed.iter().position(|&c| c == code), down) {
        (None, true) => {
            pressed.push(code);
            true
        }
        (Some(i), false) => {
            pressed.swap_remove(i);
            true
        }
        _ => false,
    }
}

// ---- Wayland state ----

struct Win {
    surface: wl_surface::WlSurface,
    xdg: XdgSurface,
    top: XdgToplevel,
    deco: Option<ZxdgToplevelDecorationV1>,
    size: (i32, i32),
    /// Latest size hint and states from `xdg_toplevel.configure`, applied at the next
    /// `xdg_surface.configure`.
    pending: Option<((i32, i32), bool, bool)>,
    active: bool,
}

/// State of the ping queue: `xdg_wm_base.ping` needs nothing but the object itself.
struct Ping;

/// The helper thread answering `xdg_wm_base.ping`; stopped and joined on drop.
struct PingThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for PingThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Serve `queue` (the `xdg_wm_base` queue) until `stop` is set or the connection fails.
fn ping_loop(conn: Connection, mut queue: EventQueue<Ping>, stop: Arc<AtomicBool>) {
    let mut st = Ping;
    while !stop.load(Ordering::Relaxed) {
        if queue.dispatch_pending(&mut st).is_err() || flush(&conn).is_err() {
            return;
        }
        let Some(guard) = queue.prepare_read() else {
            // Events are pending in libwayland's own queue: let them be dispatched.
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        };
        let mut p = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; the guard keeps the fd alive.
        if unsafe { libc::poll(&mut p, 1, 100) } > 0 {
            match guard.read() {
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
                Ok(_) => {}
            }
        } else {
            drop(guard);
        }
    }
}

struct State {
    wm_base: Option<XdgWmBase>,
    compositor: Option<wl_compositor::WlCompositor>,
    deco_mgr: Option<ZxdgDecorationManagerV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pressed: Vec<u32>,
    wins: HashMap<usize, Win>,
    out: Vec<Msg>,
}

struct Live {
    queue: EventQueue<State>,
    state: State,
}

pub struct WaylandBackend {
    conn: Connection,
    live: Option<Live>,
    ping: Option<PingThread>,
    /// A protocol or socket error happened: the backend stops dispatching.
    dead: bool,
}

impl WaylandBackend {
    /// Connect to the compositor named by the environment. An error, never a panic, when
    /// there is none.
    pub fn new() -> Result<WaylandBackend, WaylandError> {
        // `connect_to_env` panics inside libwayland-sys when the library is missing with
        // `dlopen`; the check below keeps that out of the picture.
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("WAYLAND_SOCKET").is_none()
        {
            return Err(WaylandError::Connect("WAYLAND_DISPLAY is not set".into()));
        }
        let conn = std::panic::catch_unwind(Connection::connect_to_env)
            .map_err(|_| WaylandError::Connect("libwayland-client could not be loaded".into()))?
            .map_err(|e| WaylandError::Connect(e.to_string()))?;
        Ok(WaylandBackend {
            conn,
            live: None,
            ping: None,
            dead: false,
        })
    }

    /// Bind the globals and create the event queue (first use only). False when the
    /// compositor lacks what we need.
    fn ensure_live(&mut self) -> bool {
        if self.dead {
            return false;
        }
        if self.live.is_some() {
            return true;
        }
        let (globals, mut queue) = match registry_queue_init::<State>(&self.conn) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("nativ64-gpu: wayland: registry: {e}");
                self.dead = true;
                return false;
            }
        };
        let qh = queue.handle();
        let ping_queue = self.conn.new_event_queue::<Ping>();
        let mut state = State {
            wm_base: globals.bind(&ping_queue.handle(), 1..=6, ()).ok(),
            compositor: globals.bind(&qh, 1..=6, ()).ok(),
            deco_mgr: globals.bind(&qh, 1..=1, ()).ok(),
            keyboard: None,
            pressed: Vec::new(),
            wins: HashMap::new(),
            out: Vec::new(),
        };
        if state.wm_base.is_none() || state.compositor.is_none() {
            eprintln!("nativ64-gpu: wayland: the compositor has no xdg_wm_base/wl_compositor");
            self.dead = true;
            return false;
        }
        // The seat proxy is kept by the queue; its capability events create the keyboard.
        let _seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=5, ()).ok();
        let _ = queue.roundtrip(&mut state);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (conn, stop) = (self.conn.clone(), stop.clone());
            std::thread::Builder::new()
                .name("nativ64-wl-ping".into())
                .spawn(move || ping_loop(conn, ping_queue, stop))
        };
        match handle {
            Ok(h) => {
                self.ping = Some(PingThread {
                    stop,
                    handle: Some(h),
                })
            }
            Err(e) => eprintln!("nativ64-gpu: wayland: ping thread: {e}"),
        }
        self.live = Some(Live { queue, state });
        true
    }

    fn fail(&mut self, what: &str, e: impl std::fmt::Display) {
        eprintln!("nativ64-gpu: wayland: {what}: {e}");
        self.dead = true;
    }
}

impl WindowBackend for WaylandBackend {
    fn on_create(&mut self, hwnd: usize, w: i32, h: i32, title: &str) {
        if !self.ensure_live() {
            return;
        }
        let live = self.live.as_mut().unwrap();
        let qh = live.queue.handle();
        let st = &mut live.state;
        let (Some(comp), Some(base)) = (&st.compositor, &st.wm_base) else {
            return;
        };
        let surface = comp.create_surface(&qh, ());
        let xdg = base.get_xdg_surface(&surface, &qh, hwnd);
        let top = xdg.get_toplevel(&qh, hwnd);
        top.set_title(title.to_string());
        top.set_app_id("nativ64".into());
        let deco = st.deco_mgr.as_ref().map(|m| {
            let d = m.get_toplevel_decoration(&top, &qh, ());
            d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            d
        });
        // Initial commit without a buffer: the compositor answers with a configure.
        surface.commit();
        st.wins.insert(
            hwnd,
            Win {
                surface,
                xdg,
                top,
                deco,
                size: (w, h),
                pending: None,
                active: false,
            },
        );
        if let Err(e) = flush(&self.conn) {
            self.fail("flush", e);
        }
    }

    fn on_show(&mut self, hwnd: usize, visible: bool) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let Some(win) = live.state.wins.get(&hwnd) else {
            return;
        };
        if !visible {
            // A null buffer unmaps the toplevel.
            win.surface.attach(None, 0, 0);
            win.surface.commit();
            let _ = flush(&self.conn);
        }
        // Showing needs nothing: the swapchain's next present maps the window.
    }

    fn on_destroy(&mut self, hwnd: usize) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        if let Some(w) = live.state.wins.remove(&hwnd) {
            if let Some(d) = &w.deco {
                d.destroy();
            }
            w.top.destroy();
            w.xdg.destroy();
            w.surface.destroy();
            let _ = flush(&self.conn);
        }
    }

    fn poll_events(&mut self) -> Vec<Msg> {
        let Some(live) = self.live.as_mut() else {
            return Vec::new();
        };
        if self.dead {
            return Vec::new();
        }
        let conn = &self.conn;
        let pump = |live: &mut Live| -> Result<(), String> {
            live.queue
                .dispatch_pending(&mut live.state)
                .map_err(|e| e.to_string())?;
            flush(conn)?;
            if let Some(guard) = live.queue.prepare_read() {
                let mut p = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, zero timeout.
                if unsafe { libc::poll(&mut p, 1, 0) } > 0 {
                    match guard.read() {
                        Err(wayland_client::backend::WaylandError::Io(e))
                            if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e.to_string()),
                        Ok(_) => {}
                    }
                } else {
                    drop(guard);
                }
            }
            live.queue
                .dispatch_pending(&mut live.state)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        if let Err(e) = pump(live) {
            self.fail("connection lost", e);
            return Vec::new();
        }
        std::mem::take(&mut live.state.out)
    }

    fn surface(&self, hwnd: usize) -> Option<RawSurface> {
        let w = self.live.as_ref()?.state.wins.get(&hwnd)?;
        Some(RawSurface::Wayland {
            display: self.conn.backend().display_ptr().cast(),
            surface: w.surface.id().as_ptr().cast(),
        })
    }

    fn client_size(&self, hwnd: usize) -> Option<(i32, i32)> {
        Some(self.live.as_ref()?.state.wins.get(&hwnd)?.size)
    }
}

/// Flush the outgoing requests; a full socket (`EAGAIN`) is not an error, the rest goes out
/// at the next poll.
fn flush(conn: &Connection) -> Result<(), String> {
    match conn.flush() {
        Err(wayland_client::backend::WaylandError::Io(e))
            if e.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Ok(())
        }
        Err(e) => Err(e.to_string()),
        Ok(()) => Ok(()),
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore ZxdgDecorationManagerV1);
delegate_noop!(State: ignore ZxdgToplevelDecorationV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl State {
    fn publish_keys(&self) {
        gameinput::set_keyboard_buttons(keys_mask(&self.pressed));
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let has =
            matches!(capabilities, WEnum::Value(c) if c.contains(wl_seat::Capability::Keyboard));
        match (has, st.keyboard.is_some()) {
            (true, false) => st.keyboard = Some(seat.get_keyboard(qh, ())),
            (false, true) => {
                if let Some(k) = st.keyboard.take() {
                    if k.version() >= 3 {
                        k.release();
                    }
                }
                st.pressed.clear();
                st.publish_keys();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The keymap fd is dropped (closed) with the event: raw evdev codes are used.
            wl_keyboard::Event::Keymap { .. } => {}
            wl_keyboard::Event::Enter { keys, .. } => {
                st.pressed.clear();
                for c in keys.as_chunks::<4>().0 {
                    apply_key(&mut st.pressed, u32::from_ne_bytes(*c), true);
                }
                st.publish_keys();
            }
            wl_keyboard::Event::Leave { .. } => {
                st.pressed.clear();
                st.publish_keys();
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                let down = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if apply_key(&mut st.pressed, key, down) {
                    st.publish_keys();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Ping {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, usize> for State {
    fn event(
        st: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        &hwnd: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            let Some(win) = st.wins.get_mut(&hwnd) else {
                return;
            };
            if let Some((hint, active, maximized)) = win.pending.take() {
                if let Some((m, new)) = size_msg(hwnd, win.size, hint, maximized) {
                    win.size = new;
                    st.out.push(m);
                }
                if let Some(m) = activate_msg(hwnd, win.active, active) {
                    st.out.push(m);
                }
                win.active = active;
            }
        }
    }
}

impl Dispatch<XdgToplevel, usize> for State {
    fn event(
        st: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        &hwnd: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                if let Some(win) = st.wins.get_mut(&hwnd) {
                    let (active, maximized) = decode_states(&states);
                    win.pending = Some(((width, height), active, maximized));
                }
            }
            xdg_toplevel::Event::Close => st.out.push(Msg::new(hwnd, WM_CLOSE, 0, 0)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_decode() {
        let bytes: Vec<u8> = [1u32, 4, 7].iter().flat_map(|v| v.to_ne_bytes()).collect();
        assert_eq!(decode_states(&bytes), (true, true));
        assert_eq!(decode_states(&[]), (false, false));
        assert_eq!(decode_states(&3u32.to_ne_bytes()), (false, false));
    }

    #[test]
    fn size_hint_to_wm_size() {
        // A zero hint keeps the size: no message.
        assert!(size_msg(7, (640, 480), (0, 0), false).is_none());
        assert!(size_msg(7, (640, 480), (640, 480), false).is_none());
        let (m, new) = size_msg(7, (640, 480), (800, 0), false).unwrap();
        assert_eq!(new, (800, 480));
        assert_eq!((m.hwnd, m.message, m.wparam), (7, WM_SIZE, SIZE_RESTORED));
        assert_eq!(m.lparam, 800 | (480 << 16));
        let (m, _) = size_msg(7, (640, 480), (1920, 1080), true).unwrap();
        assert_eq!(m.wparam, SIZE_MAXIMIZED);
    }

    #[test]
    fn focus_changes_only() {
        assert!(activate_msg(1, false, false).is_none());
        assert_eq!(activate_msg(1, false, true).unwrap().wparam, WA_ACTIVE);
        let m = activate_msg(1, true, false).unwrap();
        assert_eq!((m.message, m.wparam), (WM_ACTIVATE, 0));
    }

    #[test]
    fn evdev_codes_map_to_buttons() {
        use gameinput::*;
        let cases = [
            (28, A),
            (57, A),
            (1, B),
            (14, B),
            (45, X),
            (21, Y),
            (103, UP),
            (108, DOWN),
            (105, LEFT),
            (106, RIGHT),
            (15, VIEW),
            (50, MENU),
            (16, LEFT_SHOULDER),
            (18, RIGHT_SHOULDER),
            (30, 0),
            (0, 0),
            (999, 0),
        ];
        for (code, bit) in cases {
            assert_eq!(key_button(code), bit, "code {code}");
        }
    }

    #[test]
    fn pressed_set_folds_to_a_mask() {
        use gameinput::*;
        assert_eq!(keys_mask(&[]), 0);
        // Enter and Space both mean A: one bit; an unmapped key adds nothing.
        assert_eq!(keys_mask(&[28, 57, 30]), A);
        assert_eq!(keys_mask(&[103, 106, 28]), UP | RIGHT | A);
        let mut set = Vec::new();
        assert!(apply_key(&mut set, 28, true));
        assert!(!apply_key(&mut set, 28, true), "repeat changes nothing");
        assert!(apply_key(&mut set, 57, true));
        assert!(apply_key(&mut set, 28, false));
        assert_eq!(keys_mask(&set), A, "Space still holds A");
        assert!(!apply_key(&mut set, 28, false), "release of a key not held");
        assert!(apply_key(&mut set, 57, false));
        assert_eq!(keys_mask(&set), 0);
    }

    #[test]
    fn ping_thread_stops_on_drop() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        // Dropping sets the flag and joins: it returns only once the thread has exited.
        drop(PingThread {
            stop: stop.clone(),
            handle: Some(handle),
        });
        assert!(stop.load(Ordering::Relaxed));
    }

    #[test]
    fn backend_is_send() {
        fn is_send<T: Send>() {}
        is_send::<WaylandBackend>();
    }

    #[test]
    fn no_compositor_is_an_error() {
        // Only meaningful when this process has no compositor of its own to talk to.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            println!("skipped: a compositor is available (error path not testable here)");
            return;
        }
        assert!(WaylandBackend::new().is_err());
    }

    #[test]
    fn window_roundtrip_with_a_compositor() {
        let Ok(mut b) = WaylandBackend::new() else {
            println!("skipped: no Wayland compositor");
            return;
        };
        b.on_create(0x10000, 320, 200, "test");
        if b.live.is_none() {
            println!("skipped: compositor lacks xdg-shell");
            return;
        }
        assert!(matches!(
            b.surface(0x10000),
            Some(RawSurface::Wayland { .. })
        ));
        assert_eq!(b.client_size(0x10000), Some((320, 200)));
        let _ = b.poll_events();
        b.on_show(0x10000, false);
        b.on_destroy(0x10000);
        assert!(b.surface(0x10000).is_none());
    }
}
