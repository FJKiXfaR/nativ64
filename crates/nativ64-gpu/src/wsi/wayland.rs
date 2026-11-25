//! `WindowBackend` over Wayland (`wayland-client` with `system`, so the raw `wl_display *` and
//! `wl_surface *` exist for Vulkan; `libwayland-client` is opened at run time).
//!
//! Thread model: the backend sits in the global `Mutex` of `nativ64::windowing`, and every hook
//! (`on_create`, `on_show`, `on_destroy`, `poll_events`) runs on the title thread that owns the
//! window, so the event queue is only ever read from one thread at a time. The connection is
//! made in `new()` (to fail early without a compositor); globals are bound and the queue is
//! created on the first `on_create`. Nothing here blocks except that one roundtrip.
//!
//! One `xdg_toplevel` per HWND. We never attach a buffer ourselves: the window is mapped by the
//! first buffer the Vulkan swapchain presents, and `on_show(false)` unmaps it by attaching a
//! null buffer (the next present, after an initial commit and configure, maps it again).
//! The compositor's size hint (`xdg_toplevel.configure`) becomes `WM_SIZE`; the swapchain
//! picks its extent from `client_size`. Server-side decorations are requested when
//! `xdg-decoration` is offered, otherwise the window has none. Input is not handled; focus
//! comes from the `activated` toplevel state (`WM_ACTIVATE`).

use nativ64::windowing::{
    Msg, RawSurface, WindowBackend, SIZE_MAXIMIZED, SIZE_RESTORED, WA_ACTIVE, WM_ACTIVATE,
    WM_CLOSE, WM_SIZE,
};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
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

struct State {
    wm_base: Option<XdgWmBase>,
    compositor: Option<wl_compositor::WlCompositor>,
    deco_mgr: Option<ZxdgDecorationManagerV1>,
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
        let mut state = State {
            wm_base: globals.bind(&qh, 1..=6, ()).ok(),
            compositor: globals.bind(&qh, 1..=6, ()).ok(),
            deco_mgr: globals.bind(&qh, 1..=1, ()).ok(),
            wins: HashMap::new(),
            out: Vec::new(),
        };
        if state.wm_base.is_none() || state.compositor.is_none() {
            eprintln!("nativ64-gpu: wayland: the compositor has no xdg_wm_base/wl_compositor");
            self.dead = true;
            return false;
        }
        let _ = queue.roundtrip(&mut state);
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

impl Dispatch<XdgWmBase, ()> for State {
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
