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
//! - `NATIV64_INPUT_SCRIPT`, comma-separated `<mask hex>@<seconds>[+<duration>]` presses, the
//!   seconds counted from the first reading query, the duration 0.3 s by default
//!   (`4@25,20@30+1`: A at 25 s for 0.3 s, X at 30 s for one second). Bits: Menu 0x1, View 0x2,
//!   A 0x4, B 0x8, X 0x10, Y 0x20, dpad up/down/left/right 0x40/0x80/0x100/0x200. Bad entries are
//!   logged and skipped.
//! - a Linux joystick (`NATIV64_GAMEPAD`, default `/dev/input/js0` when started by the loader), see
//!   [`start_joystick`], so a real controller presses the virtual gamepad's buttons;
//! - [`set_keyboard_buttons`], for a window backend that turns key events into buttons (no
//!   backend does yet: the Wayland one has no seat).
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

/// Buttons held down on the host keyboard, set by the window backend.
static KEYBOARD_BUTTONS: AtomicU32 = AtomicU32::new(0);

/// Sets the gamepad buttons held on the host keyboard (a window backend calls this on key events).
pub fn set_keyboard_buttons(mask: u32) {
    KEYBOARD_BUTTONS.store(mask, Ordering::Relaxed);
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

/// The host joystick state as last read.
pub fn physical() -> Physical {
    *PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn update_physical(f: impl FnOnce(&mut Physical)) {
    f(&mut PHYSICAL.lock().unwrap_or_else(|e| e.into_inner()));
}

/// Forgets the host joystick state (used by the reader on device errors and by tests).
pub fn reset_physical() {
    update_physical(|p| *p = Physical::REST);
}

/// Scripted presses: (button mask, start seconds, duration seconds).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Script(Vec<(u32, f64, f64)>);

impl Script {
    /// Parses `<mask hex>@<seconds>[+<duration>]` entries separated by commas; bad entries are
    /// logged and skipped.
    pub fn parse(text: &str) -> Script {
        let mut presses = Vec::new();
        for entry in text.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match Script::entry(entry) {
                Some(press) => presses.push(press),
                None => eprintln!("nativ64: NATIV64_INPUT_SCRIPT: bad entry {entry:?} skipped"),
            }
        }
        Script(presses)
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
        self.0
            .iter()
            .filter(|(_, start, hold)| seconds >= *start && seconds < start + hold)
            .fold(0, |all, (mask, _, _)| all | mask)
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
            | physical().buttons
    }

    /// The newest reading: polls the buttons now, making a reading if they changed.
    fn poll(&self) -> Entry {
        let start = *self.start.get_or_init(Instant::now);
        self.poll_at(start.elapsed(), now_micros())
    }

    fn poll_at(&self, elapsed: Duration, timestamp: u64) -> Entry {
        let buttons = self.buttons_at(elapsed.as_secs_f64());
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        match history.last() {
            Some(last) if last.buttons == buttons => *last,
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
                entry
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
    unsafe { &(*this).data }
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

extern "win64" fn reading_kind(_: Reading) -> u32 {
    PAD_KINDS
}
extern "win64" fn reading_sequence(this: Reading, kind: u32) -> u64 {
    if kind & PAD_KINDS != 0 {
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
    // SAFETY: as above.
    unsafe { out.write(ui_navigation_buttons(buttons, &p)) };
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
/// Kinds our device does not have: nothing to count or read.
extern "win64" fn no_input(_: Reading) -> usize {
    0
}

static READING_VTBL: Table<READING_SLOTS> = table!(READING_SLOTS, Entry;
    reading_kind,      // 3
    reading_sequence,  // 4
    reading_timestamp, // 5
    reading_device,    // 6
    no_input,          // 7 raw report
    no_input,          // 8 controller axes, buttons, switches
    no_input,          // 9
    no_input,          // 10
    no_input,          // 11
    no_input,          // 12
    no_input,          // 13
    no_input,          // 14 key count
    no_input,          // 15 key state
    no_input,          // 16 mouse
    no_input,          // 17 touch
    no_input,          // 18
    no_input,          // 19 motion
    no_input,          // 20 arcade stick
    no_input,          // 21 flight stick
    gamepad_state,     // 22
    no_input,          // 23 racing wheel
    ui_navigation,     // 24 UI navigation
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

unsafe extern "win64" fn get_current_reading(
    this: This,
    kinds: u32,
    dev: *mut c_void,
    out: Out,
) -> HRESULT {
    if !dev.is_null() && dev != device() {
        return fail(out, E_INVALIDARG);
    }
    let entry = (*this).data.poll();
    hand(out, selects_pad(kinds, dev).then_some(entry))
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
    let wanted = enumeration != 0 && selects_pad(kinds, dev) && status & STATUS_CONNECTED != 0;
    eprintln!(
        "nativ64: gameinput device callback (kinds {kinds:#x}, status {status:#x}, enumeration {enumeration}): {}",
        if wanted { "the virtual gamepad" } else { "no device" }
    );
    if wanted {
        let regs = state.regs.clone();
        let (context, callback, device) = (context as usize, callback as usize, device() as usize);
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
            let Ok(name) = std::ffi::CString::new(path.clone()) else {
                return;
            };
            // SAFETY: `name` is a valid C string; a failure is handled below.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                eprintln!(
                    "nativ64: gamepad {path} cannot be opened (errno {})",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
                return;
            }
            eprintln!("nativ64: reading gamepad {path}");
            let mut buf = [0u8; 8];
            loop {
                let mut got = 0usize;
                while got < buf.len() {
                    // SAFETY: the rest of `buf` is writable and the fd is open.
                    let n = unsafe {
                        libc::read(fd, buf.as_mut_ptr().add(got).cast(), buf.len() - got)
                    };
                    if n <= 0 {
                        reset_physical();
                        return;
                    }
                    got += n as usize;
                }
                let (kind, number, value) = js_event(&buf);
                apply_js_event(kind, number, value);
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
        assert_eq!(s.0, vec![(4, 25.0, 0.3), (0x20, 30.0, 1.0), (3, 1.5, 0.5)]);
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
            assert_eq!(slot(3)(r), PAD_KINDS as u64);
            assert_eq!(slot(5)(r), 77);
            let seq: unsafe extern "win64" fn(*mut c_void, u32) -> u64 =
                crate::xgameruntime::method(r, 4);
            assert_eq!(seq(r, KIND_GAMEPAD), 9);
            assert_eq!(seq(r, 0x10), 0, "not a keyboard reading");
            let dev: unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) =
                crate::xgameruntime::method(r, 6);
            let mut d = std::ptr::null_mut();
            dev(r, &mut d);
            assert_eq!(d, device());
            assert_eq!(pad_buttons(r), B);
            // Kinds the pad does not have: no keys, no count.
            assert_eq!(slot(14)(r), 0);
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
                (0x10, 3, 2, 0x333),
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
    fn lookups_and_dispatchers() {
        unsafe {
            let (o, v) = create();
            let np = std::ptr::null_mut();
            let mut out: *mut c_void = 8 as *mut c_void;
            let r = 16 as *mut c_void;
            // Not the pad's kind, or a device that is not ours.
            assert_eq!(
                (v.get_current_reading)(o, 0x10, np, &mut out),
                GAMEINPUT_E_READING_NOT_FOUND
            );
            assert!(out.is_null());
            assert_eq!(
                (v.get_current_reading)(o, KIND_GAMEPAD, r, &mut out),
                E_INVALIDARG
            );
            assert_eq!((v.get_next_reading)(o, np, 1, np, &mut out), E_INVALIDARG);
            // NULL out pointers are tolerated.
            assert_eq!(
                (v.get_current_reading)(o, 0x10, np, std::ptr::null_mut()),
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
