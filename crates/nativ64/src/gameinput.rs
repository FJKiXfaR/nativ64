//! `GameInput.dll`: the console input API (`IGameInput`, v0), with one virtual gamepad.
//!
//! The DLL's single export is ordinal 1, `GameInputCreate(IGameInput **)`. The object has the 22
//! vtable slots of the v0 interface in published order (`IUnknown`, then GetCurrentTimestamp,
//! GetCurrentReading, GetNextReading, GetPreviousReading, GetTemporalReading,
//! RegisterReadingCallback, RegisterDeviceCallback, RegisterGuideButtonCallback,
//! RegisterKeyboardLayoutCallback, StopCallback, UnregisterCallback, CreateDispatcher,
//! CreateAggregateDevice, FindDeviceFromId, FindDeviceFromObject, FindDeviceFromPlatformHandle,
//! FindDeviceFromPlatformString, EnableOemDeviceSupport, SetFocusPolicy). The slot order, IID,
//! device and reading interfaces, and button bits come from the public GDK header as read through
//! another loader's table; no code is shared.
//!
//! The one device is a gamepad, always connected (status `0x3`: connected, input enabled), so a
//! title that registers a device callback with an enumeration is told about it once: inline for a
//! blocking enumeration, on a thread of ours for an asynchronous one. Its readings (an
//! `IGameInputReading` of kind gamepad, with `GetGamepadState`) have no sticks or triggers; the
//! buttons come from two sources, OR-ed:
//! - `NATIV64_INPUT_SCRIPT`, comma-separated entries, the times counted from the first reading
//!   query and the hold 0.3 s by default. A button press is `<mask hex>@<seconds>[+<duration>]`
//!   (`4@25,20@30+1`: A at 25 s for 0.3 s, X at 30 s for one second); bits: Menu 0x1, View 0x2,
//!   A 0x4, B 0x8, X 0x10, Y 0x20, dpad up/down/left/right 0x40/0x80/0x100/0x200, LB/RB 0x400/
//!   0x800, LS/RS 0x1000/0x2000. A stick or trigger is `<axis>=<value>@<seconds>[+<duration>]`,
//!   axis `lx`, `ly`, `rx`, `ry` (sticks, -1..1) or `lt`, `rt` (triggers, 0..1), e.g.
//!   `ry=-1@40+2` scrolls the focused panel down for two seconds. Bad entries are logged and
//!   skipped.
//! - a Linux joystick (`NATIV64_GAMEPAD`, default `/dev/input/js0` when started by the loader), see
//!   [`start_joystick`], so a real controller presses the virtual gamepad's buttons;
//! - [`set_keyboard_buttons`], for a window backend that turns key events into buttons (the
//!   Wayland one does; its key map is in `docs/library.md`). Shoulders are 0x400 / 0x800.
//!
//! A new reading is made at every change of the buttons (the history keeps the last 128), so
//! next/previous/temporal lookups work. Reading callbacks are stored and never fire; other
//! lookups of a device report `GAMEINPUT_E_DEVICE_NOT_FOUND`. Device methods and reading kinds
//! other than the gamepad are logged once and answer 0 / `E_NOTIMPL`. `GetCurrentTimestamp` is
//! microseconds on `CLOCK_MONOTONIC`.

use crate::com::{add_ref, ComObject, Guid, IUnknownVtbl, E_INVALIDARG, E_NOTIMPL, HRESULT, S_OK};
use crate::xgameruntime::{table, Table};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const GAMEINPUT_E_DEVICE_DISCONNECTED: HRESULT = 0x838A_0001u32 as i32;
pub const GAMEINPUT_E_DEVICE_NOT_FOUND: HRESULT = 0x838A_0002u32 as i32;
pub const GAMEINPUT_E_READING_NOT_FOUND: HRESULT = 0x838A_0003u32 as i32;
pub const GAMEINPUT_E_REFERENCE_READING_TOO_OLD: HRESULT = 0x838A_0004u32 as i32;

/// `{11BE2A7E-4254-445A-9C09-FFC40F006918}`
pub const IID_IGAMEINPUT: Guid = Guid::new(
    0x11be2a7e,
    0x4254,
    0x445a,
    [0x9c, 0x09, 0xff, 0xc4, 0x0f, 0x00, 0x69, 0x18],
);
static IIDS: [Guid; 1] = [IID_IGAMEINPUT];

/// Which registration function stored a callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Reading,
    Device,
    GuideButton,
    KeyboardLayout,
}

/// A stored callback registration (only device callbacks ever fire).
#[derive(Clone, Copy, Debug)]
pub struct Registration {
    pub token: u64,
    pub kind: Kind,
    pub context: usize,
    pub callback: usize,
}

/// Input kind of a gamepad, and the status of a connected device with input enabled.
pub const KIND_GAMEPAD: u32 = 0x4_0000;
/// `GameInputKindUiNavigation`: the pad also answers `GetUiNavigationState`, which the menus of
/// a title often read instead of the gamepad state.
pub const KIND_UI_NAVIGATION: u32 = 0x0100_0000;
/// What the virtual pad reports as its supported input and reading kind.
pub const PAD_KINDS: u32 = KIND_GAMEPAD | KIND_UI_NAVIGATION;
/// `GameInputKindKeyboard`: a second virtual device whose key state follows the pad's buttons
/// (the title's last screens read a keyboard and ignore the pad).
pub const KIND_KEYBOARD: u32 = 0x10;

/// `GameInputKeyState`: the scan code, code point and virtual key of one key.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KeyState {
    pub scan_code: u32,
    pub code_point: u32,
    pub virtual_key: u32,
}

/// The keys a reading's buttons stand for: `(button mask, key)`, the pad mapped to the keys the
/// Wayland seat uses (Enter for A, Escape for B, arrows for the d-pad).
const KEY_MAP: &[(u32, KeyState)] = &[
    (
        A,
        KeyState {
            scan_code: 0x1C,
            code_point: 13,
            virtual_key: 0x0D,
        },
    ),
    (
        B,
        KeyState {
            scan_code: 0x01,
            code_point: 27,
            virtual_key: 0x1B,
        },
    ),
    (
        X,
        KeyState {
            scan_code: 0x2D,
            code_point: 0x78,
            virtual_key: 0x58,
        },
    ),
    (
        Y,
        KeyState {
            scan_code: 0x15,
            code_point: 0x79,
            virtual_key: 0x59,
        },
    ),
    (
        UP,
        KeyState {
            scan_code: 0x48,
            code_point: 0,
            virtual_key: 0x26,
        },
    ),
    (
        DOWN,
        KeyState {
            scan_code: 0x50,
            code_point: 0,
            virtual_key: 0x28,
        },
    ),
    (
        LEFT,
        KeyState {
            scan_code: 0x4B,
            code_point: 0,
            virtual_key: 0x25,
        },
    ),
    (
        RIGHT,
        KeyState {
            scan_code: 0x4D,
            code_point: 0,
            virtual_key: 0x27,
        },
    ),
    (
        MENU,
        KeyState {
            scan_code: 0x01,
            code_point: 27,
            virtual_key: 0x1B,
        },
    ),
    (
        VIEW,
        KeyState {
            scan_code: 0x0F,
            code_point: 9,
            virtual_key: 0x09,
        },
    ),
];
pub const STATUS_CONNECTED: u32 = 0x3;
/// `GameInputAsyncEnumeration`; 2 is the blocking one.
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad button bits.
pub const MENU: u32 = 0x1;
pub const VIEW: u32 = 0x2;
pub const A: u32 = 0x4;
pub const B: u32 = 0x8;
pub const X: u32 = 0x10;
pub const Y: u32 = 0x20;
pub const UP: u32 = 0x40;
pub const DOWN: u32 = 0x80;
pub const LEFT: u32 = 0x100;
pub const RIGHT: u32 = 0x200;
pub const LEFT_SHOULDER: u32 = 0x400;
pub const RIGHT_SHOULDER: u32 = 0x800;

/// Buttons held down on the host keyboard, set by the window backend.
static KEYBOARD_BUTTONS: AtomicU32 = AtomicU32::new(0);

/// Sets the gamepad buttons held on the host keyboard (a window backend calls this on key events).
pub fn set_keyboard_buttons(mask: u32) {
    KEYBOARD_BUTTONS.store(mask, Ordering::Relaxed);
}

/// The buttons a gamepad in keyboard mode holds (`NATIV64_GAMEPAD_EVDEV` reads its evdev node):
/// pads that send key codes instead of `BTN_*`/ABS gamepad events show up only this way.
static EVDEV_BUTTONS: AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A Linux evdev key code (the physical position of a QWERTY key) as a gamepad button bit.
pub fn key_button(code: u32) -> u32 {
    match code {
        28 | 57 => A, // Enter, Space
        1 | 14 => B,  // Escape, Backspace
        45 => X,
        21 => Y,
        103 => UP,
        108 => DOWN,
        105 => LEFT,
        106 => RIGHT,
        15 => VIEW,           // Tab
        50 => MENU,           // M
        16 => LEFT_SHOULDER,  // Q
        18 => RIGHT_SHOULDER, // E
        // a pad that reports BTN_* gamepad codes on its evdev node (the common case on Linux)
        304 => A,              // BTN_SOUTH
        305 => B,              // BTN_EAST
        306 => X,              // BTN_WEST
        307 => Y,              // BTN_NORTH
        310 => LEFT_SHOULDER,  // BTN_TL
        311 => RIGHT_SHOULDER, // BTN_TR
        314 => VIEW,           // BTN_SELECT
        315 => MENU,           // BTN_START
        317 => LS,             // BTN_THUMBL
        318 => RS,             // BTN_THUMBR
        544 => UP,             // BTN_DPAD_UP
        545 => DOWN,           // BTN_DPAD_DOWN
        546 => LEFT,           // BTN_DPAD_LEFT
        547 => RIGHT,          // BTN_DPAD_RIGHT
        _ => 0,
    }
}

/// Starts the evdev reader of a gamepad that reports keyboard codes (`NATIV64_GAMEPAD_EVDEV`,
/// e.g. `/dev/input/event24`). Its key presses press the virtual gamepad's buttons, OR-ed with
/// the joystick, script and keyboard sources.
pub fn start_evdev(path: &str) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if path == "auto" {
        start_evdev_auto();
        return;
    }
    spawn_evdev(path.to_string());
}

/// `NATIV64_GAMEPAD_EVDEV=auto`: every input device that looks like a gamepad (a name with
/// gamepad, joystick or controller in it, or joystick axes) gets a reader, so the pad's evdev
/// node does not have to be named.
fn start_evdev_auto() {
    let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") else {
        return;
    };
    let mut found = 0;
    for block in text.split("\n\n") {
        let name = block
            .lines()
            .find_map(|l| l.strip_prefix("N: Name=\""))
            .and_then(|l| l.strip_suffix('"'))
            .unwrap_or("");
        let lower = name.to_lowercase();
        let looks_like_pad = ["gamepad", "joystick", "controller", "game pad", "pad ("]
            .iter()
            .any(|k| lower.contains(k))
            || (lower.contains("xbox") && !lower.contains("keyboard"));
        // gamepad axes: right stick (ABS_RX/RY, 3 and 4) or the D-pad hat (ABS_HAT0X/Y,
        // 16 and 17) - a touchpad has ABS_X/Y but neither
        let has_joy_axis = block
            .lines()
            .find_map(|l| l.strip_prefix("B: ABS="))
            .is_some_and(|abs| {
                let bits = abs
                    .split_whitespace()
                    .next()
                    .and_then(|w| u64::from_str_radix(w, 16).ok())
                    .unwrap_or(0);
                let stick = bits >> 3 & 1 == 1 || bits >> 4 & 1 == 1;
                let hat = bits >> 16 & 1 == 1 || bits >> 17 & 1 == 1;
                stick || hat
            });
        // the surest signal: the node carries gamepad button codes (BTN_JOYSTICK 0x120 to
        // BTN_THUMBR 0x13f) - a receiver often names its gamepad interface "... Keyboard"
        let pad_keys = block
            .lines()
            .find_map(|l| l.strip_prefix("B: KEY="))
            .is_some_and(|keys| {
                keys.split_whitespace().enumerate().any(|(wi, w)| {
                    u64::from_str_radix(w, 16).is_ok_and(|bits| {
                        (288..=319).any(|code| code / 64 == wi && bits >> (code % 64) & 1 == 1)
                    })
                })
            });
        // a pad has joystick axes or a gamepad's name; a keyboard that happens to declare
        // gamepad-like key codes has neither
        if !(looks_like_pad || has_joy_axis) || (!has_joy_axis && !pad_keys) {
            continue;
        }
        let Some(handlers) = block.lines().find_map(|l| l.strip_prefix("H: Handlers=")) else {
            continue;
        };
        let Some(node) = handlers.split_whitespace().find(|h| h.starts_with("event")) else {
            continue;
        };
        let path = format!("/dev/input/{node}");
        eprintln!("nativ64: gamepad {path} ({name})");
        spawn_evdev(path);
        found += 1;
    }
    if found == 0 {
        eprintln!("nativ64: no gamepad-like evdev node found (see /proc/bus/input/devices)");
    }
}

fn spawn_evdev(path: String) {
    std::thread::Builder::new()
        .name("nativ64-evdev".into())
        .spawn(move || {
            // the pad can sleep (a wireless one) or be re-added: the reader reopens it
            loop {
                let Ok(name) = std::ffi::CString::new(path.clone()) else {
                    return;
                };
                // SAFETY: `name` is a valid C string; a failure is handled below.
                let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
                if fd < 0 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
                eprintln!("nativ64: reading gamepad keys {path}");
                let mut buf = [0u8; 24];
                let mut alive = true;
                while alive {
                    let mut got = 0usize;
                    while got < buf.len() {
                        // SAFETY: the rest of `buf` is writable and the fd is open.
                        let n = unsafe {
                            libc::read(fd, buf.as_mut_ptr().add(got).cast(), buf.len() - got)
                        };
                        if n <= 0 {
                            alive = false;
                            break;
                        }
                        got += n as usize;
                    }
                    if !alive {
                        break;
                    }
                    // struct input_event: timeval (16 bytes), type (u16), code (u16), value (i32)
                    let kind = u16::from_le_bytes([buf[16], buf[17]]);
                    let code = u32::from(u16::from_le_bytes([buf[18], buf[19]]));
                    let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
                    if kind != 1 {
                        continue;
                    }
                    let bit = key_button(code);
                    if bit == 0 {
                        if gamepad_log() {
                            eprintln!("nativ64: gamepad key {code} (value {value}) has no button");
                        }
                        continue;
                    }
                    if gamepad_log() {
                        eprintln!("nativ64: gamepad key {code} (value {value}) -> button {bit:#x}");
                    }
                    match value {
                        1 => EVDEV_BUTTONS.fetch_or(bit, Ordering::Relaxed),
                        0 => EVDEV_BUTTONS.fetch_and(!bit, Ordering::Relaxed),
                        _ => 0,
                    };
                }
                // the device went away (or the pad slept): forget what it held
                EVDEV_BUTTONS.store(0, Ordering::Relaxed);
                // SAFETY: closing our own descriptor.
                unsafe { libc::close(fd) };
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .ok();
}

/// Whether `NATIV64_GAMEPAD_LOG` is set (prints the keys and joystick events a pad sends).
fn gamepad_log() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NATIV64_GAMEPAD_LOG").is_ok_and(|v| v != "0" && !v.is_empty())
    })
}

/// Frames the title has presented (the GPU backend reports each present).
static PRESENTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reports a presented frame: `NATIV64_INPUT_WAIT_FRAME=<n>` starts the input script's clock
/// only once the title has presented `n` frames. A start-up script pressed blind from the first
/// query misses the screens that appear at an unpredictable frame; waiting for the frame the
/// screen first showed in makes the script's seconds count from there.
pub fn note_presented_frame(frame: u64) {
    PRESENTED.store(frame, Ordering::Relaxed);
}

/// A test's overrides of the gate's frame (u64::MAX: the environment decides).
static WAIT_FRAME_FORCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// Whether the input script may run yet (a `NATIV64_INPUT_WAIT_FRAME` gate).
fn script_armed() -> bool {
    let forced = WAIT_FRAME_FORCE.load(Ordering::Relaxed);
    if forced != u64::MAX {
        return PRESENTED.load(Ordering::Relaxed) >= forced;
    }
    static WAIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let wait = *WAIT.get_or_init(|| {
        std::env::var("NATIV64_INPUT_WAIT_FRAME")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    wait == 0 || PRESENTED.load(Ordering::Relaxed) >= wait
}

/// Shoulder and stick button bits (the GDK order continues the ones above).
pub const LB: u32 = 0x400;
pub const RB: u32 = 0x800;
pub const LS: u32 = 0x1000;
pub const RS: u32 = 0x2000;

/// The host joystick's current state (buttons plus stick and trigger values); rest until a
/// joystick is read. A reading merges its buttons with the script and keyboard ones.
#[derive(Clone, Copy)]
pub struct Physical {
    pub buttons: u32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_x: f32,
    pub left_y: f32,
    pub right_x: f32,
    pub right_y: f32,
}

impl Physical {
    const REST: Physical = Physical {
        buttons: 0,
        left_trigger: 0.0,
        right_trigger: 0.0,
        left_x: 0.0,
        left_y: 0.0,
        right_x: 0.0,
        right_y: 0.0,
    };
}

static PHYSICAL: Mutex<Physical> = Mutex::new(Physical::REST);
/// The axes `NATIV64_INPUT_SCRIPT` holds right now (merged over the joystick's).
static SCRIPT_AXES: Mutex<Physical> = Mutex::new(Physical::REST);

/// The host joystick state as last read, with the scripted axes over it.
pub fn physical() -> Physical {
    let mut p = *PHYSICAL.lock().unwrap_or_else(|e| e.into_inner());
    let s = *SCRIPT_AXES.lock().unwrap_or_else(|e| e.into_inner());
    if s.left_x != 0.0 {
        p.left_x = s.left_x;
    }
    if s.left_y != 0.0 {
        p.left_y = s.left_y;
    }
    if s.right_x != 0.0 {
        p.right_x = s.right_x;
    }
    if s.right_y != 0.0 {
        p.right_y = s.right_y;
    }
    if s.left_trigger != 0.0 {
        p.left_trigger = s.left_trigger;
    }
    if s.right_trigger != 0.0 {
        p.right_trigger = s.right_trigger;
    }
    p
}

fn update_physical(f: impl FnOnce(&mut Physical)) {
    f(&mut PHYSICAL.lock().unwrap_or_else(|e| e.into_inner()));
}

/// Forgets the host joystick state (used by the reader on device errors and by tests).
pub fn reset_physical() {
    update_physical(|p| *p = Physical::REST);
}

/// Scripted input: button presses `(mask, start, hold)` and axis values `(axis, value, start,
/// hold)`, the axes numbered like [`Script::AXES`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script {
    presses: Vec<(u32, f64, f64)>,
    axes: Vec<(u8, f32, f64, f64)>,
}

impl Script {
    /// Parses `<mask hex>@<seconds>[+<duration>]` entries separated by commas; bad entries are
    /// logged and skipped.
    pub fn parse(text: &str) -> Script {
        let mut script = Script::default();
        for entry in text.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            if entry.contains('=') {
                match Script::axis_entry(entry) {
                    Some(axis) => script.axes.push(axis),
                    None => eprintln!("nativ64: NATIV64_INPUT_SCRIPT: bad entry {entry:?} skipped"),
                }
            } else {
                match Script::entry(entry) {
                    Some(press) => script.presses.push(press),
                    None => eprintln!("nativ64: NATIV64_INPUT_SCRIPT: bad entry {entry:?} skipped"),
                }
            }
        }
        script
    }

    /// The axis names of [`Script::axes_at`].
    pub const AXES: [&'static str; 6] = ["lx", "ly", "rx", "ry", "lt", "rt"];

    /// `<axis>=<value>@<seconds>[+<duration>]`.
    fn axis_entry(entry: &str) -> Option<(u8, f32, f64, f64)> {
        let (name, rest) = entry.split_once('=')?;
        let axis = Script::AXES.iter().position(|a| *a == name.trim())? as u8;
        let (value, when) = rest.split_once('@')?;
        let value: f32 = value.trim().parse().ok()?;
        let (start, hold) = match when.split_once('+') {
            Some((start, hold)) => (start, hold.parse::<f64>().ok()?),
            None => (when, 0.3),
        };
        let start: f64 = start.parse().ok()?;
        (start >= 0.0
            && hold > 0.0
            && start.is_finite()
            && hold.is_finite()
            && value.is_finite()
            && (-1.0..=1.0).contains(&value))
        .then_some((axis, value, start, hold))
    }

    fn entry(entry: &str) -> Option<(u32, f64, f64)> {
        let (mask, when) = entry.split_once('@')?;
        let mask = mask.trim_start_matches("0x");
        let (start, hold) = match when.split_once('+') {
            Some((start, hold)) => (start, hold.parse::<f64>().ok()?),
            None => (when, 0.3),
        };
        let (mask, start) = (
            u32::from_str_radix(mask, 16).ok()?,
            start.parse::<f64>().ok()?,
        );
        (start >= 0.0 && hold > 0.0 && start.is_finite() && hold.is_finite())
            .then_some((mask, start, hold))
    }

    /// The buttons held `seconds` after the first reading query.
    pub fn buttons_at(&self, seconds: f64) -> u32 {
        self.presses
            .iter()
            .filter(|(_, start, hold)| seconds >= *start && seconds < start + hold)
            .fold(0, |all, (mask, _, _)| all | mask)
    }

    /// The axes held `seconds` after the first reading query (rest where none is scripted).
    pub fn axes_at(&self, seconds: f64) -> Physical {
        let mut p = Physical::REST;
        for (axis, value, start, hold) in &self.axes {
            if seconds < *start || seconds >= start + hold {
                continue;
            }
            match axis {
                0 => p.left_x = *value,
                1 => p.left_y = *value,
                2 => p.right_x = *value,
                3 => p.right_y = *value,
                4 => p.left_trigger = *value,
                _ => p.right_trigger = *value,
            }
        }
        p
    }
}

/// One reading of the gamepad: a new one is made at every change of the buttons.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub sequence: u64,
    pub timestamp: u64,
    pub buttons: u32,
}

/// Readings kept for next / previous / temporal lookups.
const HISTORY: usize = 128;

/// Time since `CLOCK_MONOTONIC` zero in microseconds.
fn now_micros() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

pub struct State {
    next_token: AtomicU64,
    regs: Arc<Mutex<Vec<Registration>>>,
    script: Script,
    /// The first reading query: the script's time zero.
    start: OnceLock<Instant>,
    history: Mutex<Vec<Entry>>,
    next_sequence: AtomicU64,
}

impl Default for State {
    /// No script and no clock yet; only the keyboard can press anything.
    fn default() -> State {
        State::new(Script::default())
    }
}

impl State {
    pub fn new(script: Script) -> State {
        State {
            next_token: AtomicU64::new(0),
            regs: Arc::default(),
            script,
            start: OnceLock::new(),
            history: Mutex::default(),
            next_sequence: AtomicU64::new(1),
        }
    }

    /// The buttons held at `seconds` after the clock started.
    fn buttons_at(&self, seconds: f64) -> u32 {
        self.script.buttons_at(seconds)
            | KEYBOARD_BUTTONS.load(Ordering::Relaxed)
            | EVDEV_BUTTONS.load(Ordering::Relaxed)
            | physical().buttons
    }

    /// The newest reading: polls the buttons now, making a reading if they changed.
    fn poll(&self) -> Entry {
        if !script_armed() {
            // the scripted pad is at rest until the gate's frame (a script that has to wait for
            // the title to reach a screen), but the player's own pad and keyboard are not
            return self.poll_gated(now_micros());
        }
        let start = *self.start.get_or_init(Instant::now);
        self.poll_at(start.elapsed(), now_micros())
    }

    /// A poll before the script's clock starts: only the sources that are not the script.
    fn poll_gated(&self, timestamp: u64) -> Entry {
        *SCRIPT_AXES.lock().unwrap_or_else(|e| e.into_inner()) = Physical::REST;
        let buttons = KEYBOARD_BUTTONS.load(Ordering::Relaxed)
            | EVDEV_BUTTONS.load(Ordering::Relaxed)
            | physical().buttons;
        self.push_reading(buttons, timestamp)
    }

    /// Makes a reading of `buttons` when they differ from the newest one, hands it to the
    /// title's callbacks and returns the newest reading.
    fn push_reading(&self, buttons: u32, timestamp: u64) -> Entry {
        let fresh = {
            let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            match history.last() {
                Some(last) if last.buttons == buttons => None,
                _ => {
                    let entry = Entry {
                        sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
                        timestamp,
                        buttons,
                    };
                    eprintln!("nativ64: gamepad buttons {buttons:#x}");
                    history.push(entry);
                    if history.len() > HISTORY {
                        history.remove(0);
                    }
                    Some(entry)
                }
            }
        };
        if let Some(entry) = fresh {
            self.fire_reading_callbacks(entry);
            entry
        } else {
            *self
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .expect("an entry")
        }
    }

    fn poll_at(&self, elapsed: Duration, timestamp: u64) -> Entry {
        let seconds = elapsed.as_secs_f64();
        let buttons = self.buttons_at(seconds);
        *SCRIPT_AXES.lock().unwrap_or_else(|e| e.into_inner()) = self.script.axes_at(seconds);
        let fresh = {
            let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            match history.last() {
                Some(last) if last.buttons == buttons => None,
                _ => {
                    let entry = Entry {
                        sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
                        timestamp,
                        buttons,
                    };
                    eprintln!(
                        "nativ64: gamepad buttons {buttons:#x} at {:.2} s after the first query",
                        elapsed.as_secs_f64()
                    );
                    history.push(entry);
                    if history.len() > HISTORY {
                        history.remove(0);
                    }
                    Some(entry)
                }
            }
        };
        if let Some(entry) = fresh {
            // the history lock is released before the title's callbacks run (they may query a
            // reading again)
            self.fire_reading_callbacks(entry);
            entry
        } else {
            *self
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .expect("an entry")
        }
    }

    /// The new reading goes to every `RegisterReadingCallback` registration, as the title sees it
    /// (the reading is valid for the call; the caller keeps its own references).
    fn fire_reading_callbacks(&self, entry: Entry) {
        let targets: Vec<(u64, usize, usize)> = self
            .regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|r| matches!(r.kind, Kind::Reading))
            .map(|r| (r.token, r.context, r.callback))
            .collect();
        for (token, context, callback) in targets {
            let reading = new_reading(entry);
            // SAFETY: the callback the title registered, with its own context and a reading of
            // ours; the reading is released afterwards as the API's borrowed one is.
            unsafe {
                let f: unsafe extern "win64" fn(u64, *mut c_void, *mut c_void) =
                    std::mem::transmute(callback);
                f(token, context as *mut c_void, reading);
                release_reading(reading);
            }
        }
    }

    fn history(&self) -> Vec<Entry> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn register(
        &self,
        kind: Kind,
        context: *mut c_void,
        callback: *const c_void,
        out: *mut u64,
    ) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed) + 1;
        self.regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Registration {
                token,
                kind,
                context: context as usize,
                callback: callback as usize,
            });
        if !out.is_null() {
            // SAFETY: non-null; the title passes room for a token.
            unsafe { out.write(token) };
        }
        token
    }
    fn forget(&self, token: u64) -> bool {
        let mut regs = self.regs.lock().unwrap_or_else(|e| e.into_inner());
        let before = regs.len();
        regs.retain(|r| r.token != token);
        regs.len() != before
    }
}

pub type Object = ComObject<Vtbl, State>;
type This = *mut Object;
type Out = *mut *mut c_void;

/// Stored registrations of an object (tests and diagnostics).
///
/// # Safety
/// `this` is a live GameInput object.
pub unsafe fn registrations(this: *mut Object) -> Vec<Registration> {
    // SAFETY: a live GameInput object.
    unsafe {
        (*this)
            .data
            .regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// `GameInputDeviceInfo`: what a title reads from `GetDeviceInfo`.
#[repr(C)]
#[derive(Default)]
pub struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers: none.
    rest: [u64; 26],
}
const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

const DEVICE_SLOTS: usize = 21;
type DeviceObject = ComObject<Table<DEVICE_SLOTS>, DeviceInfo>;

/// The virtual gamepad: made once, with a reference of its own so a title's last release of the
/// reference it was handed does not free it.
static DEVICE: LazyLock<usize> = LazyLock::new(|| {
    let mut id = [0u8; 32];
    id[..7].copy_from_slice(b"nativ64");
    id[31] = 1;
    let info = DeviceInfo {
        size: size_of::<DeviceInfo>() as u32,
        id,
        root_id: id,
        supported_input: PAD_KINDS,
        // Low and high frequency.
        rumble_motors: 0x3,
        ..DeviceInfo::default()
    };
    let device = DeviceObject::create(&DEVICE_VTBL, &[], info);
    // SAFETY: just made.
    unsafe { add_ref(device) };
    device as usize
});

/// The virtual gamepad, as the pointer titles get.
pub fn device() -> *mut c_void {
    *DEVICE as *mut c_void
}

/// The virtual keyboard: a second device of `GameInputKindKeyboard` whose key state is the pad's
/// buttons (see [`KEY_MAP`]). It exists because the title's last start-up screens ask for a
/// keyboard device (the device callback for `GameInputKindKeyboard` used to answer no device).
static KEYBOARD_DEVICE: LazyLock<usize> = LazyLock::new(|| {
    let mut id = [0u8; 32];
    id[..14].copy_from_slice(b"nativ64-keybrd");
    id[31] = 2;
    let info = DeviceInfo {
        size: size_of::<DeviceInfo>() as u32,
        id,
        root_id: id,
        supported_input: KIND_KEYBOARD,
        ..DeviceInfo::default()
    };
    let device = DeviceObject::create(&DEVICE_VTBL, &[], info);
    // SAFETY: just made.
    unsafe { add_ref(device) };
    device as usize
});

/// Whether the keyboard device is announced (`NATIV64_KEYBOARD_DEVICE=1`): with it there the
/// title moves its menus onto the keyboard reading, whose keys it reads in a way not matched
/// yet, so the pad-only path stays the default until that is understood.
static KEYBOARD_FORCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn keyboard_announced() -> bool {
    // 0: the environment decides (once), 1: forced on by a test, 2: forced off
    match KEYBOARD_FORCE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NATIV64_KEYBOARD_DEVICE").is_ok_and(|v| v != "0" && !v.is_empty())
    })
}

/// Forces the keyboard device on or off (tests; the environment decides in a run).
pub fn set_keyboard_announced(on: bool) {
    KEYBOARD_FORCE.store(if on { 1 } else { 2 }, std::sync::atomic::Ordering::Relaxed);
}

/// The virtual keyboard, as the pointer titles get.
pub fn keyboard_device() -> *mut c_void {
    *KEYBOARD_DEVICE as *mut c_void
}

/// `NATIV64_KB_LOG=1`: print what the title asks the keyboard device for (throttled per call
/// site), so a run says why a menu ignores the keys.
fn kb_log(what: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("NATIV64_KB_LOG").is_some()) {
        return;
    }
    static SEEN: std::sync::Mutex<[u32; 8]> = std::sync::Mutex::new([0; 8]);
    let slot = match what {
        "device info" => 0,
        "current reading" => 1,
        "key count" => 2,
        "key state" => 3,
        "reading callback" => 4,
        _ => 5,
    };
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen[slot] += 1;
    if seen[slot] <= 8 {
        eprintln!("nativ64: kb {what} #{}", seen[slot]);
    }
}

/// The keys a reading holds down, from its buttons.
fn keys_of(buttons: u32) -> Vec<KeyState> {
    KEY_MAP
        .iter()
        .filter(|(mask, _)| buttons & mask != 0)
        .map(|(_, key)| *key)
        .collect()
}

type Device = *mut DeviceObject;

static LOGGED: Mutex<Vec<(u8, usize)>> = Mutex::new(Vec::new());

/// A device or reading method not implemented: logged once, answers 0 (or `E_NOTIMPL`).
extern "win64" fn unsupported<const WHICH: u8, const SLOT: usize>(_: *mut c_void) -> HRESULT {
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if !logged.contains(&(WHICH, SLOT)) {
        logged.push((WHICH, SLOT));
        let what = if WHICH == 0 { "device" } else { "reading" };
        eprintln!(
            "nativ64: unimplemented gameinput {what} method, slot {SLOT} (offset {:#x})",
            SLOT * 8
        );
    }
    E_NOTIMPL
}

extern "win64" fn device_info(this: Device) -> *const DeviceInfo {
    // SAFETY: the device object.
    let info = unsafe { &(*this).data };
    kb_log(if info.supported_input == KIND_KEYBOARD {
        "device info (keyboard)"
    } else {
        "device info"
    });
    info
}
extern "win64" fn device_status(_: Device) -> u32 {
    STATUS_CONNECTED
}
/// Rumble: there is no motor to drive.
extern "win64" fn set_rumble(_: Device, _: *const f32) {}

static DEVICE_VTBL: Table<DEVICE_SLOTS> = table!(DEVICE_SLOTS, DeviceInfo;
    device_info,                // 3
    device_status,              // 4
    unsupported::<0, 5>,        // battery
    unsupported::<0, 6>,        // force feedback
    unsupported::<0, 7>,
    unsupported::<0, 8>,
    unsupported::<0, 9>,        // haptics
    set_rumble,                 // 10
    unsupported::<0, 11>,       // input synchronization
    unsupported::<0, 12>,
    unsupported::<0, 13>,       // power off
    unsupported::<0, 14>,       // raw device access
    unsupported::<0, 15>,
    unsupported::<0, 16>,
    unsupported::<0, 17>,
    unsupported::<0, 18>,
    unsupported::<0, 19>,
    unsupported::<0, 20>,
);

/// A gamepad reading as `GetGamepadState` gives it (sticks and triggers stay at rest).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}
const _: () = assert!(size_of::<PadState>() == 28);

/// `GameInputUiNavigationButtons` bits (the gamepad is turned into them: a title's menus can
/// read this instead of the gamepad state).
pub const UI_MENU: u32 = 0x1;
pub const UI_VIEW: u32 = 0x2;
pub const UI_ACCEPT: u32 = 0x4;
pub const UI_CANCEL: u32 = 0x8;
pub const UI_UP: u32 = 0x10;
pub const UI_DOWN: u32 = 0x20;
pub const UI_LEFT: u32 = 0x40;
pub const UI_RIGHT: u32 = 0x80;
pub const UI_CONTEXT1: u32 = 0x100;
pub const UI_CONTEXT2: u32 = 0x200;
pub const UI_CONTEXT3: u32 = 0x400;
pub const UI_CONTEXT4: u32 = 0x800;
pub const UI_PAGE_UP: u32 = 0x1000;
pub const UI_PAGE_DOWN: u32 = 0x2000;
pub const UI_PAGE_LEFT: u32 = 0x4000;
pub const UI_PAGE_RIGHT: u32 = 0x8000;
pub const UI_SCROLL_UP: u32 = 0x1_0000;
pub const UI_SCROLL_DOWN: u32 = 0x2_0000;
pub const UI_SCROLL_LEFT: u32 = 0x4_0000;
pub const UI_SCROLL_RIGHT: u32 = 0x8_0000;

/// Turns the pad's buttons, sticks and triggers into `GameInputUiNavigationState`: A accepts, B
/// cancels, X/Y/LS/RS are context 1-4, the D-pad and the left stick past half are the
/// directions, the shoulders page, the triggers page up/down and the right stick scrolls.
fn ui_navigation_buttons(buttons: u32, p: &Physical) -> u32 {
    let mut ui = 0;
    macro_rules! map {
        ($pad:expr, $ui:expr) => {
            if buttons & $pad != 0 {
                ui |= $ui;
            }
        };
    }
    map!(MENU, UI_MENU);
    map!(VIEW, UI_VIEW);
    map!(A, UI_ACCEPT);
    map!(B, UI_CANCEL);
    map!(Y, UI_CONTEXT2);
    map!(X, UI_CONTEXT1);
    map!(LS, UI_CONTEXT3);
    map!(RS, UI_CONTEXT4);
    map!(LB, UI_PAGE_LEFT);
    map!(RB, UI_PAGE_RIGHT);
    // Directions: the D-pad, or the left stick past half.
    if buttons & UP != 0 || p.left_y > 0.5 {
        ui |= UI_UP;
    }
    if buttons & DOWN != 0 || p.left_y < -0.5 {
        ui |= UI_DOWN;
    }
    if buttons & LEFT != 0 || p.left_x < -0.5 {
        ui |= UI_LEFT;
    }
    if buttons & RIGHT != 0 || p.left_x > 0.5 {
        ui |= UI_RIGHT;
    }
    // Triggers page up/down, the right stick scrolls.
    if p.left_trigger > 0.5 {
        ui |= UI_PAGE_UP;
    }
    if p.right_trigger > 0.5 {
        ui |= UI_PAGE_DOWN;
    }
    if p.right_y > 0.5 {
        ui |= UI_SCROLL_UP;
    }
    if p.right_y < -0.5 {
        ui |= UI_SCROLL_DOWN;
    }
    if p.right_x < -0.5 {
        ui |= UI_SCROLL_LEFT;
    }
    if p.right_x > 0.5 {
        ui |= UI_SCROLL_RIGHT;
    }
    ui
}

const READING_SLOTS: usize = 25;
type ReadingObject = ComObject<Table<READING_SLOTS>, Entry>;
type Reading = *mut ReadingObject;

/// A new reading object (one reference) for `entry`.
fn new_reading(entry: Entry) -> *mut c_void {
    ReadingObject::create(&READING_VTBL, &[], entry) as *mut c_void
}

/// Drops a reference of a reading this module made.
fn release_reading(reading: *mut c_void) {
    // SAFETY: a reading of ours.
    unsafe { crate::com::release(reading as Reading) };
}

extern "win64" fn reading_kind(_: Reading) -> u32 {
    PAD_KINDS | KIND_KEYBOARD
}
extern "win64" fn reading_sequence(this: Reading, kind: u32) -> u64 {
    if kind & (PAD_KINDS | KIND_KEYBOARD) != 0 {
        // SAFETY: a reading we handed out.
        unsafe { (*this).data.sequence }
    } else {
        0
    }
}
extern "win64" fn reading_timestamp(this: Reading) -> u64 {
    // SAFETY: a reading we handed out.
    unsafe { (*this).data.timestamp }
}
extern "win64" fn reading_device(_: Reading, out: *mut *mut c_void) {
    if !out.is_null() {
        // SAFETY: non-null; devices need no reference.
        unsafe { out.write(device()) };
    }
}
/// `GetUiNavigationState(state)`: the pad as the UI-navigation buttons a title's menus read.
extern "win64" fn ui_navigation(this: Reading, out: *mut u32) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: a reading we handed out; `out` has room for the state.
    let buttons = unsafe { (*this).data.buttons };
    let p = physical();
    let ui = ui_navigation_buttons(buttons, &p);
    // `NATIV64_UI_LOG=1` prints each change of the state, with the time since the first query:
    // the input scripts press blind, this says what the title's menus actually consumed.
    if std::env::var_os("NATIV64_UI_LOG").is_some() {
        static LAST: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let t0 = *START.get_or_init(std::time::Instant::now);
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if *last != Some(ui) {
            eprintln!(
                "nativ64: ui navigation 0x{ui:03x} at {:.2} s",
                t0.elapsed().as_secs_f64()
            );
            *last = Some(ui);
        }
    }
    // SAFETY: as above.
    unsafe { out.write(ui) };
    true
}

extern "win64" fn gamepad_state(this: Reading, out: *mut PadState) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: a reading we handed out; `out` has room for the state.
    let p = physical();
    unsafe {
        out.write(PadState {
            // SAFETY: as above.
            buttons: (*this).data.buttons,
            left_trigger: p.left_trigger,
            right_trigger: p.right_trigger,
            left_x: p.left_x,
            left_y: p.left_y,
            right_x: p.right_x,
            right_y: p.right_y,
        })
    };
    true
}
/// `IGameInputReading` method names by slot (the public v0 order, XCL's facts for the API).
fn reading_method(slot: usize) -> &'static str {
    match slot {
        7 => "GetRawReport",
        8 => "GetControllerAxisCount",
        9 => "GetControllerAxisState",
        10 => "GetControllerButtonCount",
        11 => "GetControllerButtonState",
        12 => "GetControllerSwitchCount",
        13 => "GetControllerSwitchState",
        14 => "GetKeyCount",
        15 => "GetKeyState",
        16 => "GetMouseState",
        17 => "GetTouchCount",
        18 => "GetTouchState",
        19 => "GetMotionState",
        20 => "GetArcadeStickState",
        21 => "GetFlightStickState",
        22 => "GetGamepadState",
        23 => "GetRacingWheelState",
        24 => "GetUiNavigationState",
        _ => "?",
    }
}

/// A reading method this pad does not have: logged once (so a run names what the title asked
/// for), then answers 0/NULL like an absent input kind.
extern "win64" fn reading_trap<const SLOT: usize>(_: Reading) -> usize {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_or(1 << SLOT, Ordering::Relaxed) & (1 << SLOT) == 0 {
        eprintln!(
            "nativ64: gameinput reading {} (slot {SLOT}) has no input to report",
            reading_method(SLOT)
        );
    }
    0
}

/// `GetKeyCount()`: how many keys the reading holds down.
extern "win64" fn key_count(this: Reading) -> u32 {
    // SAFETY: a reading we handed out.
    let buttons = unsafe { (*this).data.buttons };
    kb_log("key count");
    keys_of(buttons).len() as u32
}
/// `GetKeyState(index, out)`: one of the keys the reading holds down.
extern "win64" fn key_state(this: Reading, index: u32, out: *mut KeyState) -> bool {
    // SAFETY: a reading we handed out.
    let buttons = unsafe { (*this).data.buttons };
    let keys = keys_of(buttons);
    match keys.get(index as usize) {
        Some(key) if !out.is_null() => {
            // SAFETY: non-null out with room for the state.
            unsafe { out.write(*key) };
            true
        }
        _ => false,
    }
}

static READING_VTBL: Table<READING_SLOTS> = table!(READING_SLOTS, Entry;
    reading_kind,      // 3
    reading_sequence,  // 4
    reading_timestamp, // 5
    reading_device,    // 6
    reading_trap::<7>,  // 7 raw report
    reading_trap::<8>,  // 8 controller axis count
    reading_trap::<9>,  // 9 controller axis state
    reading_trap::<10>, // 10 controller button count
    reading_trap::<11>, // 11 controller button state
    reading_trap::<12>, // 12 controller switch count
    reading_trap::<13>, // 13 controller switch state
    key_count,          // 14 key count
    key_state,          // 15 key state
    reading_trap::<16>, // 16 mouse state
    reading_trap::<17>, // 17 touch count
    reading_trap::<18>, // 18 touch state
    reading_trap::<19>, // 19 motion state
    reading_trap::<20>, // 20 arcade stick state
    reading_trap::<21>, // 21 flight stick state
    gamepad_state,      // 22
    reading_trap::<23>, // 23 racing wheel state
    ui_navigation,      // 24 UI navigation state
);

#[repr(C)]
pub struct Vtbl {
    unknown: IUnknownVtbl<Vtbl, State>,
    get_current_timestamp: unsafe extern "win64" fn(This) -> u64,
    get_current_reading: unsafe extern "win64" fn(This, u32, *mut c_void, Out) -> HRESULT,
    get_next_reading: unsafe extern "win64" fn(This, *mut c_void, u32, *mut c_void, Out) -> HRESULT,
    get_previous_reading:
        unsafe extern "win64" fn(This, *mut c_void, u32, *mut c_void, Out) -> HRESULT,
    get_temporal_reading: unsafe extern "win64" fn(This, u64, *mut c_void, Out) -> HRESULT,
    // (device, kinds, threshold, context, callback, token out)
    register_reading_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        u32,
        f32,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    // (device, kinds, status filter, async enumeration, context, callback, token out)
    register_device_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        u32,
        u32,
        u32,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    register_guide_button_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    register_keyboard_layout_callback: unsafe extern "win64" fn(
        This,
        *mut c_void,
        *mut c_void,
        *const c_void,
        *mut u64,
    ) -> HRESULT,
    stop_callback: unsafe extern "win64" fn(This, u64),
    unregister_callback: unsafe extern "win64" fn(This, u64, u64) -> u8,
    create_dispatcher: unsafe extern "win64" fn(This, Out) -> HRESULT,
    create_aggregate_device: unsafe extern "win64" fn(This, u32, Out) -> HRESULT,
    find_device_from_id: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_object: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_platform_handle: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    find_device_from_platform_string: unsafe extern "win64" fn(This, *const c_void, Out) -> HRESULT,
    enable_oem_device_support: unsafe extern "win64" fn(This, u16, u16, u8, u8) -> HRESULT,
    set_focus_policy: unsafe extern "win64" fn(This, u32),
}

fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    hr
}

unsafe extern "win64" fn get_current_timestamp(_: This) -> u64 {
    now_micros()
}
/// Hands `entry` to the title as a new reading, or reports there is none.
fn hand(out: Out, entry: Option<Entry>) -> HRESULT {
    match entry {
        Some(entry) => {
            if out.is_null() {
                return S_OK;
            }
            // SAFETY: non-null; the title passes room for a pointer.
            unsafe { out.write(new_reading(entry)) };
            S_OK
        }
        None => fail(out, GAMEINPUT_E_READING_NOT_FOUND),
    }
}

/// Whether a query for `kinds` on `device` (NULL: any) reaches the gamepad.
fn selects_pad(kinds: u32, device: *mut c_void) -> bool {
    kinds & PAD_KINDS != 0 && (device.is_null() || device == self::device())
}

/// Whether a query for `kinds` on `device` (NULL: any) reaches the virtual keyboard.
fn selects_keyboard(kinds: u32, device: *mut c_void) -> bool {
    keyboard_announced()
        && kinds & KIND_KEYBOARD != 0
        && (device.is_null() || device == keyboard_device())
}

unsafe extern "win64" fn get_current_reading(
    this: This,
    kinds: u32,
    dev: *mut c_void,
    out: Out,
) -> HRESULT {
    if !dev.is_null() && dev != device() && dev != keyboard_device() {
        return fail(out, E_INVALIDARG);
    }
    let entry = (*this).data.poll();
    let wanted = selects_pad(kinds, dev) || selects_keyboard(kinds, dev);
    if kinds & KIND_KEYBOARD != 0 {
        kb_log("current reading (keyboard)");
    }
    // every kind the title asks for (a mouse query, say, is a device we do not have)
    if std::env::var_os("NATIV64_KB_LOG").is_some() {
        static KINDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        let mut seen = KINDS.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&kinds) {
            eprintln!("nativ64: kb current reading kinds {kinds:#x} (device {dev:p})");
            seen.push(kinds);
        }
    }
    hand(out, wanted.then_some(entry))
}
/// The reading after (`forward`) or before `reference`, in the history.
unsafe fn relative(
    this: This,
    reference: *mut c_void,
    kinds: u32,
    dev: *mut c_void,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() || (!dev.is_null() && dev != device()) {
        return fail(out, E_INVALIDARG);
    }
    let state = &(*this).data;
    state.poll();
    let sequence = (*(reference as Reading)).data.sequence;
    let history = state.history();
    let found = if forward {
        history.iter().find(|e| e.sequence > sequence)
    } else {
        history.iter().rev().find(|e| e.sequence < sequence)
    };
    hand(out, found.copied().filter(|_| selects_pad(kinds, dev)))
}
unsafe extern "win64" fn get_next_reading(
    this: This,
    reference: *mut c_void,
    kinds: u32,
    dev: *mut c_void,
    out: Out,
) -> HRESULT {
    relative(this, reference, kinds, dev, out, true)
}
unsafe extern "win64" fn get_previous_reading(
    this: This,
    reference: *mut c_void,
    kinds: u32,
    dev: *mut c_void,
    out: Out,
) -> HRESULT {
    relative(this, reference, kinds, dev, out, false)
}
/// The last reading made at or before `time` (microseconds on the timestamp clock).
unsafe extern "win64" fn get_temporal_reading(
    this: This,
    time: u64,
    dev: *mut c_void,
    out: Out,
) -> HRESULT {
    if !dev.is_null() && dev != device() {
        return fail(out, E_INVALIDARG);
    }
    let state = &(*this).data;
    state.poll();
    let found = state
        .history()
        .into_iter()
        .rev()
        .find(|e| e.timestamp <= time);
    hand(out, found)
}
unsafe extern "win64" fn register_reading_callback(
    this: This,
    _dev: *mut c_void,
    _kinds: u32,
    _threshold: f32,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this).data.register(Kind::Reading, context, callback, out);
    kb_log("reading callback");
    S_OK
}
/// (token, context, device, timestamp, current status, previous status)
type DeviceCallback = unsafe extern "win64" fn(u64, *mut c_void, *mut c_void, u64, u32, u32);

/// Reports the connected gamepad to the new registration when it asked for an enumeration: before
/// returning (blocking), or from a thread of ours (asynchronous).
unsafe extern "win64" fn register_device_callback(
    this: This,
    dev: *mut c_void,
    kinds: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    if callback.is_null() {
        return E_INVALIDARG;
    }
    let state = &(*this).data;
    let token = state.register(Kind::Device, context, callback, out);
    let pad = enumeration != 0 && selects_pad(kinds, dev) && status & STATUS_CONNECTED != 0;
    let keyboard =
        enumeration != 0 && selects_keyboard(kinds, dev) && status & STATUS_CONNECTED != 0;
    let wanted = pad || keyboard;
    eprintln!(
        "nativ64: gameinput device callback (kinds {kinds:#x}, status {status:#x}, enumeration {enumeration}): {}",
        match (pad, keyboard) {
            (true, true) => "the virtual gamepad and keyboard",
            (true, false) => "the virtual gamepad",
            (false, true) => "the virtual keyboard",
            (false, false) => "no device",
        }
    );
    if wanted {
        let regs = state.regs.clone();
        let device = if pad { device() } else { keyboard_device() } as usize;
        let (context, callback) = (context as usize, callback as usize);
        let deliver = move || {
            let live = regs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|r| r.token == token);
            if live {
                // SAFETY: the callback the title registered with its context.
                let callback: DeviceCallback = unsafe { std::mem::transmute(callback) };
                unsafe {
                    callback(
                        token,
                        context as *mut c_void,
                        device as *mut c_void,
                        now_micros(),
                        STATUS_CONNECTED,
                        0,
                    )
                };
            }
        };
        match enumeration {
            ASYNC_ENUMERATION => {
                std::thread::spawn(deliver);
            }
            _ => deliver(),
        }
    }
    S_OK
}
unsafe extern "win64" fn register_guide_button_callback(
    this: This,
    _dev: *mut c_void,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this)
        .data
        .register(Kind::GuideButton, context, callback, out);
    S_OK
}
unsafe extern "win64" fn register_keyboard_layout_callback(
    this: This,
    _dev: *mut c_void,
    context: *mut c_void,
    callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    (*this)
        .data
        .register(Kind::KeyboardLayout, context, callback, out);
    S_OK
}
unsafe extern "win64" fn stop_callback(this: This, token: u64) {
    (*this).data.forget(token);
}
// (token, timeout in microseconds) -> true when no callback is still running.
unsafe extern "win64" fn unregister_callback(this: This, token: u64, _timeout: u64) -> u8 {
    (*this).data.forget(token);
    1
}
unsafe extern "win64" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    fail(out, E_NOTIMPL)
}
unsafe extern "win64" fn create_aggregate_device(_: This, _kinds: u32, out: Out) -> HRESULT {
    fail(out, E_NOTIMPL)
}
unsafe extern "win64" fn find_device(_: This, value: *const c_void, out: Out) -> HRESULT {
    if value.is_null() {
        return fail(out, E_INVALIDARG);
    }
    fail(out, GAMEINPUT_E_DEVICE_NOT_FOUND)
}
unsafe extern "win64" fn enable_oem_device_support(
    _: This,
    _v: u16,
    _p: u16,
    _i: u8,
    _c: u8,
) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn set_focus_policy(_: This, _policy: u32) {}

static VTBL: Vtbl = Vtbl {
    unknown: IUnknownVtbl::new(),
    get_current_timestamp,
    get_current_reading,
    get_next_reading,
    get_previous_reading,
    get_temporal_reading,
    register_reading_callback,
    register_device_callback,
    register_guide_button_callback,
    register_keyboard_layout_callback,
    stop_callback,
    unregister_callback,
    create_dispatcher,
    create_aggregate_device,
    find_device_from_id: find_device,
    find_device_from_object: find_device,
    find_device_from_platform_handle: find_device,
    find_device_from_platform_string: find_device,
    enable_oem_device_support,
    set_focus_policy,
};

/// Ordinals of `GameInput.dll`: `(ordinal, name)`.
pub static ORDINALS: &[(u16, &str)] = &[(1, "GameInputCreate")];

win32! {
    module "gameinput";
    // Creates an input object with one reference (one virtual gamepad).
    Native fn GameInputCreate(out: *mut *mut Object) -> HRESULT {
        if out.is_null() {
            return E_INVALIDARG;
        }
        let script = match std::env::var("NATIV64_INPUT_SCRIPT") {
            Ok(text) => Script::parse(&text),
            Err(_) => Script::default(),
        };
        *out = Object::create(&VTBL, &IIDS, State::new(script));
        S_OK
    }
}

/// Starts the Linux joystick reader: a background thread reads `struct js_event` records from
/// the given device (the loader passes `NATIV64_GAMEPAD`, default `/dev/input/js0`) and keeps
/// [`PHYSICAL`] current, so a real controller presses the virtual gamepad's buttons and moves its
/// sticks. Idempotent; a device that cannot be opened is logged once.
pub fn start_joystick(path: Option<&str>) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let path = path.unwrap_or("/dev/input/js0").to_string();
    std::thread::Builder::new()
        .name("nativ64-joystick".into())
        .spawn(move || {
            // a controller can be switched on after the title has started, and a wireless one
            // sleeps: the reader keeps reopening it
            loop {
                let Ok(name) = std::ffi::CString::new(path.clone()) else {
                    return;
                };
                // SAFETY: `name` is a valid C string; a failure retries below.
                let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
                if fd < 0 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
                eprintln!("nativ64: reading gamepad {path}");
                let mut buf = [0u8; 8];
                let mut alive = true;
                while alive {
                    let mut got = 0usize;
                    while got < buf.len() {
                        // SAFETY: the rest of `buf` is writable and the fd is open.
                        let n = unsafe {
                            libc::read(fd, buf.as_mut_ptr().add(got).cast(), buf.len() - got)
                        };
                        if n <= 0 {
                            alive = false;
                            break;
                        }
                        got += n as usize;
                    }
                    if !alive {
                        break;
                    }
                    let (kind, number, value) = js_event(&buf);
                    if gamepad_log() && value != 0 {
                        eprintln!("nativ64: gamepad js kind {kind} number {number} value {value}");
                    }
                    apply_js_event(kind, number, value);
                }
                reset_physical();
                // SAFETY: closing our own descriptor.
                unsafe { libc::close(fd) };
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .ok();
}

/// One `struct js_event`: `(type, number, value)` (the time field is not needed).
fn js_event(b: &[u8; 8]) -> (u8, u8, i16) {
    (b[6] & 0x7f, b[7], i16::from_le_bytes([b[4], b[5]]))
}

const JS_BUTTON: u8 = 1;
const JS_AXIS: u8 = 2;

/// Joystick button number -> gamepad button bit (the xpad mapping of an Xbox 360 pad).
fn button_bit(number: u8) -> Option<u32> {
    Some(match number {
        0 => A,
        1 => B,
        2 => X,
        3 => Y,
        4 => LB,
        5 => RB,
        6 => VIEW,
        7 => MENU,
        9 => LS,
        10 => RS,
        _ => return None,
    })
}

/// Applies one joystick event to [`PHYSICAL`]: buttons, the D-pad hat and the sticks/triggers.
fn apply_js_event(kind: u8, number: u8, value: i16) {
    match kind {
        JS_BUTTON => {
            let Some(bit) = button_bit(number) else {
                return;
            };
            update_physical(|p| {
                if value != 0 {
                    p.buttons |= bit;
                } else {
                    p.buttons &= !bit;
                }
            });
        }
        JS_AXIS => {
            // Axes 6 and 7 are the D-pad hat: left/right and up/down (negative is up/left).
            if matches!(number, 6 | 7) {
                let (neg, pos) = if number == 6 {
                    (LEFT, RIGHT)
                } else {
                    (UP, DOWN)
                };
                update_physical(|p| {
                    p.buttons &= !(neg | pos);
                    if value < -16_000 {
                        p.buttons |= neg;
                    } else if value > 16_000 {
                        p.buttons |= pos;
                    }
                });
                return;
            }
            let stick = (f32::from(value) / 32_767.0).clamp(-1.0, 1.0);
            let trigger = ((i32::from(value) + 32_767) as f32 / 65_534.0).clamp(0.0, 1.0);
            update_physical(|p| match number {
                0 => p.left_x = stick,
                1 => p.left_y = -stick,
                3 => p.right_x = stick,
                4 => p.right_y = -stick,
                2 => p.left_trigger = trigger,
                5 => p.right_trigger = trigger,
                _ => {}
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame gate holds only the script: a player's keyboard still presses the pad.
    #[test]
    fn the_frame_gate_does_not_hold_the_players_input() {
        let mut st = State::default();
        st.script = Script::parse("0x4@1+2");
        WAIT_FRAME_FORCE.store(1_000_000, Ordering::Relaxed);
        note_presented_frame(10);
        set_keyboard_buttons(A);
        let entry = st.poll();
        assert_eq!(entry.buttons, A, "the keyboard's A before the gate's frame");
        set_keyboard_buttons(0);
        let entry = st.poll();
        assert_eq!(entry.buttons, 0);
        WAIT_FRAME_FORCE.store(u64::MAX, Ordering::Relaxed);
    }

    /// One test only: the physical state is process-wide.
    #[test]
    fn ui_navigation_maps_the_pad() {
        let rest = Physical::REST;
        assert_eq!(
            ui_navigation_buttons(A | B | X | Y, &rest),
            UI_ACCEPT | UI_CANCEL | UI_CONTEXT1 | UI_CONTEXT2
        );
        assert_eq!(
            ui_navigation_buttons(UP | DOWN | LEFT | RIGHT, &rest),
            UI_UP | UI_DOWN | UI_LEFT | UI_RIGHT
        );
        assert_eq!(
            ui_navigation_buttons(MENU | VIEW | LS | RS, &rest),
            UI_MENU | UI_VIEW | UI_CONTEXT3 | UI_CONTEXT4
        );
        assert_eq!(
            ui_navigation_buttons(LB | RB, &rest),
            UI_PAGE_LEFT | UI_PAGE_RIGHT
        );
        // Sticks and triggers past half act even with no button pressed.
        let mut p = Physical::REST;
        p.left_x = -0.9;
        p.left_y = 0.9;
        p.right_x = 0.9;
        p.right_y = -0.9;
        p.left_trigger = 0.8;
        p.right_trigger = 0.6;
        assert_eq!(
            ui_navigation_buttons(0, &p),
            UI_LEFT | UI_UP | UI_SCROLL_RIGHT | UI_SCROLL_DOWN | UI_PAGE_UP | UI_PAGE_DOWN
        );
        // A small stick or trigger is not a press.
        let mut p = Physical::REST;
        p.left_x = 0.3;
        p.left_trigger = 0.3;
        assert_eq!(ui_navigation_buttons(0, &p), 0);
    }

    #[test]
    fn joystick_events_press_buttons_and_move_sticks() {
        reset_physical();
        // A press (button 0, value 1), the D-pad hat up (axis 7, negative), a stick and a trigger.
        apply_js_event(JS_BUTTON, 0, 1);
        apply_js_event(JS_AXIS, 7, -32_767);
        apply_js_event(JS_AXIS, 0, 32_767);
        apply_js_event(JS_AXIS, 1, 32_767);
        apply_js_event(JS_AXIS, 2, 32_767);
        let p = physical();
        assert_eq!(p.buttons & A, A);
        assert_eq!(p.buttons & UP, UP);
        assert!((p.left_x - 1.0).abs() < 1e-6);
        assert!((p.left_y + 1.0).abs() < 1e-6, "stick Y is positive up");
        assert!((p.left_trigger - 1.0).abs() < 1e-3);
        // Release and the D-pad to the right.
        apply_js_event(JS_BUTTON, 0, 0);
        apply_js_event(JS_AXIS, 7, 0);
        apply_js_event(JS_AXIS, 6, 32_767);
        let p = physical();
        assert_eq!(p.buttons & A, 0);
        assert_eq!(p.buttons & UP, 0);
        assert_eq!(p.buttons & RIGHT, RIGHT);
        // An unknown button number changes nothing.
        let before = physical().buttons;
        apply_js_event(JS_BUTTON, 42, 1);
        assert_eq!(physical().buttons, before);
        reset_physical();
        assert_eq!(physical().buttons, 0);
        // The event record is little-endian: time, value, type, number.
        assert_eq!(
            js_event(&[0, 0, 0, 0, 0xff, 0x7f, 0x81, 0]),
            (JS_BUTTON, 0, 32_767)
        );
    }

    use crate::com::{E_NOINTERFACE, E_POINTER, IID_IUNKNOWN};

    fn create() -> (*mut Object, &'static Vtbl) {
        announce_keyboard();
        let mut o: *mut Object = std::ptr::null_mut();
        assert_eq!(unsafe { GameInputCreate(&mut o) }, S_OK);
        assert!(!o.is_null());
        (o, unsafe { &*(*(o as *const *const Vtbl)) })
    }

    #[test]
    fn create_null_out_and_ordinal() {
        assert_eq!(
            unsafe { GameInputCreate(std::ptr::null_mut()) },
            E_INVALIDARG
        );
        let e = crate::lookup_ordinal("GameInput.dll", 1).unwrap();
        assert_eq!(e.name, "GameInputCreate");
        assert!(crate::lookup("GameInput.dll", "GameInputCreate").is_some());
        assert!(crate::lookup_ordinal("GameInput.dll", 2).is_none());
    }

    #[test]
    fn query_and_release() {
        unsafe {
            let (o, v) = create();
            let mut out = std::ptr::null_mut();
            for iid in [IID_IGAMEINPUT, IID_IUNKNOWN] {
                assert_eq!((v.unknown.query_interface)(o, &iid, &mut out), S_OK);
                assert_eq!(out, o);
            }
            assert_eq!((*o).ref_count(), 3);
            let bad = Guid::new(1, 1, 1, [1; 8]);
            assert_eq!(
                (v.unknown.query_interface)(o, &bad, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                (v.unknown.query_interface)(o, &bad, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!((v.unknown.release)(o), 2);
            assert_eq!((v.unknown.release)(o), 1);
            assert_eq!((v.unknown.add_ref)(o), 2);
            assert_eq!((v.unknown.release)(o), 1);
            assert_eq!((v.unknown.release)(o), 0);
        }
    }

    /// The state of a reading object through its vtable.
    unsafe fn pad_buttons(reading: *mut c_void) -> u32 {
        let get: unsafe extern "win64" fn(*mut c_void, *mut PadState) -> bool =
            crate::xgameruntime::method(reading, 22);
        let mut st = PadState {
            buttons: 0xdead,
            ..PadState::default()
        };
        assert!(get(reading, &mut st));
        assert_eq!(st.left_trigger, 0.0);
        st.buttons
    }

    unsafe fn release_reading(reading: *mut c_void) -> u32 {
        let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
            crate::xgameruntime::method(reading, 2);
        rel(reading)
    }

    #[test]
    fn script_parsing_and_timing() {
        let s = Script::parse("4@25,0x20@30+1, 3@1.5+0.5 ,bad,4@,@3,zz@1,4@-1,4@1+0");
        assert_eq!(
            s.presses,
            vec![(4, 25.0, 0.3), (0x20, 30.0, 1.0), (3, 1.5, 0.5)]
        );
        assert_eq!(s.buttons_at(0.0), 0);
        assert_eq!(s.buttons_at(1.5), 3);
        assert_eq!(s.buttons_at(1.99), 3);
        assert_eq!(s.buttons_at(2.0), 0);
        assert_eq!(s.buttons_at(25.0), A);
        assert_eq!(s.buttons_at(25.31), 0);
        assert_eq!(s.buttons_at(30.9), Y);
        // Overlapping presses are OR-ed.
        assert_eq!(Script::parse("4@1+2,8@2").buttons_at(2.1), A | B);
    }

    #[test]
    fn scripted_axes_are_parsed_and_held() {
        let s = Script::parse("ry=-1@40+2,rx=0.5@1,lt=1@5+0.5,bad=1@1,bogus,ry=2@1");
        assert_eq!(s.axes_at(40.5).right_y, -1.0);
        assert_eq!(s.axes_at(42.0).right_y, 0.0);
        assert_eq!(s.axes_at(1.1).right_x, 0.5);
        assert_eq!(s.axes_at(5.2).left_trigger, 1.0);
        // a value outside -1..=1 is a bad entry and is skipped
        assert!(s
            .axes
            .iter()
            .all(|(_, value, _, _)| (-1.0..=1.0).contains(value)));
    }

    #[test]
    fn a_scripted_axis_shows_through_the_joystick_state() {
        *SCRIPT_AXES.lock().unwrap_or_else(|e| e.into_inner()) = Physical {
            right_y: -1.0,
            ..Physical::REST
        };
        assert_eq!(physical().right_y, -1.0);
        *SCRIPT_AXES.lock().unwrap_or_else(|e| e.into_inner()) = Physical::REST;
        assert_eq!(physical().right_y, 0.0);
    }

    #[test]
    fn readings_follow_the_script_and_the_history() {
        unsafe {
            let (o, v) = create();
            (*o).data = State::new(Script::parse("4@1+1,20@3"));
            let np = std::ptr::null_mut();
            let st = &(*o).data;
            // Drive the clock by hand: three polls at 0 s, 1.5 s and 2.5 s, then 3.1 s.
            let at = |s: f64, t| st.poll_at(Duration::from_secs_f64(s), t);
            let (e0, e1) = (at(0.0, 100), at(1.5, 200));
            assert_eq!((e0.buttons, e1.buttons), (0, A));
            assert_eq!(at(1.7, 250), e1, "no change, same reading");
            let e2 = at(2.5, 300);
            let e3 = at(3.1, 400);
            assert_eq!((e2.buttons, e3.buttons), (0, Y));
            assert_eq!(
                [e0.sequence, e1.sequence, e2.sequence, e3.sequence],
                [1, 2, 3, 4]
            );

            let mut out: *mut c_void = 8 as *mut c_void;
            assert_eq!((v.get_current_reading)(o, KIND_GAMEPAD, np, &mut out), S_OK);
            let cur = out;
            // The real clock is near 0 s: no press yet.
            assert_eq!(pad_buttons(cur), 0);
            // Temporal: the reading at or before time 250 is e1's; before everything is none.
            assert_eq!((v.get_temporal_reading)(o, 250, np, &mut out), S_OK);
            assert_eq!(pad_buttons(out), A);
            assert_eq!(release_reading(out), 0);
            assert_eq!(
                (v.get_temporal_reading)(o, 50, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert!(out.is_null());
            // Previous / next of the oldest reading reached by the history.
            let r0 = new_reading(e1);
            assert_eq!(
                (v.get_next_reading)(o, r0, KIND_GAMEPAD, np, &mut out),
                S_OK
            );
            assert_eq!(pad_buttons(out), 0);
            assert_eq!(release_reading(out), 0);
            assert_eq!(
                (v.get_previous_reading)(o, r0, KIND_GAMEPAD, np, &mut out),
                S_OK
            );
            assert_eq!(pad_buttons(out), 0);
            release_reading(out);
            assert_eq!(release_reading(r0), 0);
            let newest = new_reading(st.poll());
            assert_eq!(
                (v.get_next_reading)(o, newest, KIND_GAMEPAD, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            release_reading(newest);
            release_reading(cur);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn reading_objects_describe_themselves() {
        unsafe {
            let r = new_reading(Entry {
                sequence: 9,
                timestamp: 77,
                buttons: B,
            });
            let slot = |n| {
                crate::xgameruntime::method::<unsafe extern "win64" fn(*mut c_void) -> u64>(r, n)
            };
            assert_eq!(slot(3)(r), (PAD_KINDS | KIND_KEYBOARD) as u64);
            assert_eq!(slot(5)(r), 77);
            let seq: unsafe extern "win64" fn(*mut c_void, u32) -> u64 =
                crate::xgameruntime::method(r, 4);
            assert_eq!(seq(r, KIND_GAMEPAD), 9);
            // the keyboard kind answers too now (the reading is the pad's state as keys)
            assert_eq!(seq(r, KIND_KEYBOARD), 9);
            let dev: unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) =
                crate::xgameruntime::method(r, 6);
            let mut d = std::ptr::null_mut();
            dev(r, &mut d);
            assert_eq!(d, device());
            assert_eq!(pad_buttons(r), B);
            // B is the keyboard's Escape: one key down.
            assert_eq!(slot(14)(r), 1);
            let state: unsafe extern "win64" fn(*mut c_void, *mut PadState) -> bool =
                crate::xgameruntime::method(r, 22);
            assert!(!state(r, std::ptr::null_mut()));
            // The object counts references.
            let add: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(r, 1);
            assert_eq!(add(r), 2);
            assert_eq!(release_reading(r), 1);
            assert_eq!(release_reading(r), 0);
        }
    }

    #[test]
    fn the_device_is_a_connected_gamepad() {
        unsafe {
            let d = device();
            assert_eq!(d, device(), "one device");
            let info: unsafe extern "win64" fn(*mut c_void) -> *const DeviceInfo =
                crate::xgameruntime::method(d, 3);
            let status: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(d, 4);
            let i = &*info(d);
            assert_eq!(i.size as usize, size_of::<DeviceInfo>());
            assert_eq!(i.supported_input, PAD_KINDS);
            assert_eq!(i.rumble_motors, 3);
            assert_eq!(status(d), STATUS_CONNECTED);
            let rumble: unsafe extern "win64" fn(*mut c_void, *const f32) =
                crate::xgameruntime::method(d, 10);
            rumble(d, [0.5f32; 4].as_ptr());
            // A balanced AddRef / Release leaves the device alive.
            let add: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(d, 1);
            let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(d, 2);
            add(d);
            rel(d);
            assert_eq!(status(d), STATUS_CONNECTED);
            let battery: unsafe extern "win64" fn(*mut c_void) -> HRESULT =
                crate::xgameruntime::method(d, 5);
            assert_eq!(battery(d), E_NOTIMPL);
        }
    }

    static READ: Mutex<Vec<(u64, usize, u32)>> = Mutex::new(Vec::new());
    unsafe extern "win64" fn on_reading(token: u64, ctx: *mut c_void, reading: *mut c_void) {
        let kind: unsafe extern "win64" fn(*mut c_void) -> u32 =
            crate::xgameruntime::method(reading, 3);
        READ.lock()
            .unwrap()
            .push((token, ctx as usize, kind(reading)));
    }

    /// The tests run with the keyboard device announced (the lookups and reading tests expect
    /// it); a run leaves the environment to decide.
    fn announce_keyboard() {
        set_keyboard_announced(true);
    }

    #[test]
    fn a_reading_callback_sees_each_change() {
        unsafe {
            let (o, v) = create();
            (*o).data = State::new(Script::parse("4@1+1"));
            let mut token = 0u64;
            assert_eq!(
                (v.register_reading_callback)(
                    o,
                    std::ptr::null_mut(),
                    KIND_GAMEPAD,
                    0.0,
                    0x77 as *mut c_void,
                    on_reading as *const c_void,
                    &mut token
                ),
                S_OK
            );
            let st = &(*o).data;
            st.poll_at(Duration::ZERO, 1);
            st.poll_at(Duration::from_secs_f64(1.5), 2);
            let seen = READ.lock().unwrap().clone();
            let mine: Vec<_> = seen.iter().filter(|e| e.1 == 0x77).collect();
            // the keyboard mask is global (another test may press it at the same time): the
            // first change at least is ours
            assert!(mine.len() >= 2, "a change for the rest state and for A");
            assert_eq!(
                mine[1].2 & KIND_KEYBOARD,
                KIND_KEYBOARD,
                "the reading is a keyboard too"
            );
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn keyboard_buttons_press_the_pad() {
        let st = State::default();
        set_keyboard_buttons(A | UP);
        let held = st.poll_at(Duration::ZERO, 1);
        set_keyboard_buttons(0);
        assert_eq!(held.buttons, A | UP);
        assert_eq!(st.poll_at(Duration::ZERO, 2).buttons, 0);
    }

    type Seen = (u64, usize, usize, u32, u32);
    static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());
    unsafe extern "win64" fn on_device(
        token: u64,
        ctx: *mut c_void,
        dev: *mut c_void,
        _t: u64,
        now: u32,
        before: u32,
    ) {
        SEEN.lock()
            .unwrap()
            .push((token, ctx as usize, dev as usize, now, before));
    }

    #[test]
    fn device_callbacks_enumerate_the_pad() {
        unsafe {
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let cb = on_device as *const c_void;
            let seen = |ctx: usize| {
                SEEN.lock()
                    .unwrap()
                    .iter()
                    .filter(|e| e.1 == ctx)
                    .copied()
                    .collect::<Vec<_>>()
            };
            // Blocking enumeration: delivered before the call returns.
            let mut t = 0;
            assert_eq!(
                (v.register_device_callback)(
                    o,
                    np,
                    KIND_GAMEPAD,
                    0x3,
                    2,
                    0x111 as *mut c_void,
                    cb,
                    &mut t
                ),
                S_OK
            );
            assert_eq!(
                seen(0x111),
                vec![(t, 0x111, device() as usize, STATUS_CONNECTED, 0)]
            );
            // No enumeration, a keyboard-only filter, another device, a status filter that excludes
            // connected devices: nothing is delivered.
            for (kinds, status, enumeration, ctx) in [
                (KIND_GAMEPAD, 3, 0, 0x222),
                (KIND_KEYBOARD, 0, 2, 0x333),
                (KIND_GAMEPAD, 0, 2, 0x444),
            ] {
                assert_eq!(
                    (v.register_device_callback)(
                        o,
                        np,
                        kinds,
                        status,
                        enumeration,
                        ctx as *mut c_void,
                        cb,
                        std::ptr::null_mut()
                    ),
                    S_OK
                );
                assert!(seen(ctx).is_empty(), "{ctx:#x}");
            }
            assert_eq!(
                (v.register_device_callback)(
                    o,
                    0x99 as *mut c_void,
                    KIND_GAMEPAD,
                    3,
                    2,
                    0x555 as *mut c_void,
                    cb,
                    &mut t
                ),
                S_OK
            );
            assert!(seen(0x555).is_empty());
            // A NULL callback is refused.
            assert_eq!(
                (v.register_device_callback)(o, np, 1, 3, 2, np, std::ptr::null(), &mut t),
                E_INVALIDARG
            );
            // Asynchronous enumeration: delivered from another thread.
            assert_eq!(
                (v.register_device_callback)(
                    o,
                    np,
                    KIND_GAMEPAD,
                    3,
                    1,
                    0x666 as *mut c_void,
                    cb,
                    &mut t
                ),
                S_OK
            );
            for _ in 0..200 {
                if !seen(0x666).is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(seen(0x666).len(), 1);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn the_keyboard_device_reports_the_pads_keys() {
        unsafe {
            let r = new_reading(Entry {
                sequence: 1,
                timestamp: 2,
                buttons: A | DOWN,
            });
            let count: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(r, 14);
            assert_eq!(count(r), 2);
            let key: unsafe extern "win64" fn(*mut c_void, u32, *mut KeyState) -> bool =
                crate::xgameruntime::method(r, 15);
            let mut state = KeyState::default();
            assert!(key(r, 0, &mut state));
            assert_eq!(
                (state.virtual_key, state.code_point),
                (0x0D, 13),
                "Enter for A"
            );
            assert!(key(r, 1, &mut state));
            assert_eq!(state.virtual_key, 0x28, "the down arrow");
            assert!(!key(r, 2, &mut state));
            // the reading is a reference of its own: release it through its own vtable
            let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(r, 2);
            rel(r);
        }
    }

    #[test]
    fn lookups_and_dispatchers() {
        unsafe {
            announce_keyboard();
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let mut out: *mut c_void = 8 as *mut c_void;
            let r = 16 as *mut c_void;
            // A keyboard reading is the pad's keys.
            assert_eq!(
                (v.get_current_reading)(o, KIND_KEYBOARD, np, &mut out),
                S_OK
            );
            assert!(!out.is_null());
            let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                crate::xgameruntime::method(out, 2);
            rel(out);
            out = std::ptr::null_mut();
            // A device that is not ours.
            assert_eq!(
                (v.get_current_reading)(o, KIND_GAMEPAD, r, &mut out),
                E_INVALIDARG
            );
            assert!(out.is_null());
            assert_eq!(
                (v.get_current_reading)(o, KIND_GAMEPAD, r, &mut out),
                E_INVALIDARG
            );
            assert_eq!((v.get_next_reading)(o, np, 1, np, &mut out), E_INVALIDARG);
            // NULL out pointers are tolerated; a mouse is not ours.
            assert_eq!(
                (v.get_current_reading)(o, 0x20, np, std::ptr::null_mut()),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert_eq!((v.create_dispatcher)(o, &mut out), E_NOTIMPL);
            assert_eq!((v.create_aggregate_device)(o, 1, &mut out), E_NOTIMPL);
            assert_eq!(
                (v.find_device_from_id)(o, r, &mut out),
                GAMEINPUT_E_DEVICE_NOT_FOUND
            );
            assert_eq!((v.find_device_from_id)(o, np, &mut out), E_INVALIDARG);
            assert_eq!((v.enable_oem_device_support)(o, 1, 2, 3, 4), S_OK);
            (v.set_focus_policy)(o, 1);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn timestamp_is_monotonic() {
        unsafe {
            let (o, v) = create();
            let a = (v.get_current_timestamp)(o);
            std::thread::sleep(std::time::Duration::from_millis(2));
            let b = (v.get_current_timestamp)(o);
            assert!(b >= a + 1500, "{a} {b}");
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn callbacks_get_tokens() {
        unsafe {
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let (mut t1, mut t2, mut t3) = (0u64, 0u64, 0u64);
            let cb = 0x1234 as *const c_void;
            assert_eq!(
                (v.register_device_callback)(o, np, 1, 3, 0, np, cb, &mut t1),
                S_OK
            );
            assert_eq!(
                (v.register_reading_callback)(o, np, 1, 0.0, np, cb, &mut t2),
                S_OK
            );
            assert_eq!(
                (v.register_guide_button_callback)(o, np, np, cb, &mut t3),
                S_OK
            );
            assert_eq!(
                (v.register_keyboard_layout_callback)(o, np, np, cb, std::ptr::null_mut()),
                S_OK
            );
            assert!(t1 != 0 && t2 != t1 && t3 != t2);
            assert_eq!(registrations(o).len(), 4);
            assert_eq!(registrations(o)[0].kind, Kind::Device);
            (v.stop_callback)(o, t1);
            assert_eq!((v.unregister_callback)(o, t2, 1000), 1);
            assert_eq!((v.unregister_callback)(o, 999, 0), 1);
            assert_eq!(registrations(o).len(), 2);
            (v.unknown.release)(o);
        }
    }

    #[test]
    fn concurrent_refcount() {
        unsafe {
            let (o, v) = create();
            let p = o as usize;
            let hs: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || {
                        let o = p as *mut Object;
                        let v = &**(o as *const *const Vtbl);
                        for _ in 0..1000 {
                            (v.unknown.add_ref)(o);
                            (v.unknown.release)(o);
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            assert_eq!((*o).ref_count(), 1);
            assert_eq!((v.unknown.release)(o), 0);
        }
    }
}
