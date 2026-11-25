//! `xapu.dll`: the console's audio decoder queue (see `docs/audio.md`).
//!
//! A title connects once, gets a block of memory for its packets and output, queues per-stream
//! commands (activate, deactivate, process, notify, reset) and dequeues one result per command.
//! Every command completes when it is queued, so its result is ready at once and results come
//! in command order. There is no XMA or Opus decoding yet: a process command writes silence of
//! the length it asked for and reports the output as ready and more input wanted.

use crate::com::{E_INVALIDARG, HRESULT, S_OK};
use crate::handles::{self, Handle};
use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

pub const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as i32;
pub const XAPU_QUEUE_FULL: HRESULT = 0x8AC8_1000u32 as i32;
/// `XApuDequeueResult` with nothing to return, which titles poll for.
pub const XAPU_NO_RESULTS: HRESULT = 0x8AC8_1001u32 as i32;
pub const XAPU_INVALID_COMMAND: HRESULT = 0x8AC8_1002u32 as i32;
pub const XAPU_INVALID_STREAM: HRESULT = 0x8AC8_1003u32 as i32;

pub const ACTIVATE: u32 = 1;
pub const DEACTIVATE: u32 = 2;
pub const PROCESS: u32 = 3;
pub const NOTIFY: u32 = 4;
pub const RESET: u32 = 5;

/// Command option: signal the connection's event when the result is ready.
pub const OPT_SIGNAL: u32 = 1;
/// Connect option: create the result event.
pub const CONNECT_EVENT: u32 = 8;
/// Result state bits: output ready, more input wanted.
pub const STATE_READY: u32 = 1;
pub const STATE_WANT_INPUT: u32 = 2;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ConnectInput {
    pub process: u32,
    pub streams: u32,
    pub commands_per_stream: u32,
    pub data_bytes: u32,
    pub frames_per_output: u32,
    pub options: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ConnectOutput {
    pub data: *mut c_void,
    pub ready: usize,
}

/// What every command starts with and its result echoes: 24 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct CommandId {
    pub kind: u32,
    pub stream: u32,
    pub sequence: u32,
    pub options: u32,
    pub context: u64,
}

/// Activate: the id, then `channels` (1 or 2) at +24.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Activate {
    pub id: CommandId,
    pub channels: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Process {
    pub id: CommandId,
    pub ramp: f32,
    pub pitch: f32,
    pub first_frame: u32,
    pub frames: u32,
    pub input_bytes: u32,
    pub input: *mut c_void,
    pub output_bytes: u32,
    pub output: *mut c_void,
}

/// 80 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct XApuResult {
    pub id: CommandId,
    pub state: u32,
    pub state_data: u64,
    pub time: u64,
    pub error: HRESULT,
    pub frames: u32,
    pub output_bytes: u32,
    pub output: *mut c_void,
    pub pitch: f32,
    pub input_bytes: u32,
    pub input: *mut c_void,
}

struct Stream {
    channels: u32,
    active: bool,
    queued: u32,
}

struct State {
    streams: Vec<Stream>,
    results: VecDeque<XApuResult>,
    enqueued: u32,
    dequeued: u32,
}

pub struct Connection {
    data: *mut u8,
    layout: Option<Layout>,
    event: Handle,
    per_stream: u32,
    frames_per_output: u32,
    state: Mutex<State>,
}

fn state(c: &Connection) -> MutexGuard<'_, State> {
    c.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Silence for `frames` of `channels` float32 samples; the frame count the result reports.
unsafe fn process(c: &Connection, s: &Stream, p: &Process, r: &mut XApuResult) {
    let channels = s.channels.max(1);
    let asked = if p.frames != 0 {
        p.frames
    } else {
        c.frames_per_output
    };
    let asked = if asked != 0 { asked } else { 960 };
    let frames = asked.min(p.output_bytes / (channels * 4));
    let bytes = frames * channels * 4;
    if !p.output.is_null() && bytes != 0 {
        std::ptr::write_bytes(p.output.cast::<u8>(), 0, bytes as usize);
    }
    r.state = STATE_READY | STATE_WANT_INPUT;
    r.frames = frames;
    r.output_bytes = bytes;
    r.output = p.output;
    r.pitch = p.pitch;
    r.input_bytes = p.input_bytes;
    r.input = p.input;
}

win32! {
    module "xapu";
    // (&input, &output, &handle)
    Native fn XApuConnect(input: *const ConnectInput, output: *mut ConnectOutput,
                          handle: *mut *mut Connection) -> HRESULT {
        if input.is_null() || output.is_null() || handle.is_null() {
            return E_INVALIDARG;
        }
        let i = input.read_unaligned();
        if !(1..=500).contains(&{ i.streams }) {
            return E_INVALIDARG;
        }
        let (data, layout) = if i.data_bytes != 0 {
            let Ok(layout) = Layout::from_size_align(i.data_bytes as usize, 0x1_0000) else {
                return E_INVALIDARG;
            };
            let p = std::alloc::alloc_zeroed(layout);
            if p.is_null() {
                return E_OUTOFMEMORY;
            }
            (p, Some(layout))
        } else {
            (std::ptr::null_mut(), None)
        };
        let event = if i.options & CONNECT_EVENT != 0 {
            handles::create_event(None, false, false)
        } else {
            0
        };
        let streams = (0..i.streams)
            .map(|_| Stream { channels: 2, active: false, queued: 0 })
            .collect();
        let c = Box::new(Connection {
            data,
            layout,
            event,
            per_stream: i.commands_per_stream,
            frames_per_output: i.frames_per_output,
            state: Mutex::new(State {
                streams,
                results: VecDeque::new(),
                enqueued: 0,
                dequeued: 0,
            }),
        });
        output.write_unaligned(ConnectOutput { data: data.cast(), ready: event });
        handle.write(Box::into_raw(c));
        S_OK
    }
    Native fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
        if handle.is_null() {
            return E_INVALIDARG;
        }
        let c = Box::from_raw(handle);
        if c.event != 0 {
            handles::close_handle(c.event);
        }
        if let Some(layout) = c.layout {
            std::alloc::dealloc(c.data, layout);
        }
        S_OK
    }
    // (handle, &command, &total): `total` receives the number of commands queued so far.
    Native fn XApuEnqueueCommand(handle: *mut Connection, command: *const CommandId,
                                 total: *mut u32) -> HRESULT {
        if handle.is_null() || command.is_null() {
            return E_INVALIDARG;
        }
        let c = &*handle;
        let id = command.read_unaligned();
        let mut st = state(c);
        let n = id.stream as usize;
        if n >= st.streams.len() {
            return XAPU_INVALID_STREAM;
        }
        if !(ACTIVATE..=RESET).contains(&{ id.kind }) {
            return XAPU_INVALID_COMMAND;
        }
        if c.per_stream != 0 && st.streams[n].queued >= c.per_stream {
            return XAPU_QUEUE_FULL;
        }
        let mut r = XApuResult {
            id,
            state: 0,
            state_data: 0,
            time: 0,
            error: S_OK,
            frames: 0,
            output_bytes: 0,
            output: std::ptr::null_mut(),
            pitch: 1.0,
            input_bytes: 0,
            input: std::ptr::null_mut(),
        };
        match id.kind {
            ACTIVATE => {
                let channels = command.cast::<Activate>().read_unaligned().channels;
                if (1..=2).contains(&channels) {
                    st.streams[n].channels = channels;
                    st.streams[n].active = true;
                } else {
                    r.error = E_INVALIDARG;
                }
            }
            DEACTIVATE => st.streams[n].active = false,
            PROCESS => {
                if st.streams[n].active {
                    let p = command.cast::<Process>().read_unaligned();
                    process(c, &st.streams[n], &p, &mut r);
                } else {
                    r.error = XAPU_INVALID_STREAM;
                }
            }
            _ => {}
        }
        st.streams[n].queued += 1;
        st.enqueued = st.enqueued.wrapping_add(1);
        st.results.push_back(r);
        if !total.is_null() {
            total.write(st.enqueued);
        }
        if id.options & OPT_SIGNAL != 0 && c.event != 0 {
            handles::set_event(c.event);
        }
        S_OK
    }
    // (handle, &result): `XAPU_NO_RESULTS` when the queue is empty.
    Native fn XApuDequeueResult(handle: *mut Connection, out: *mut XApuResult) -> HRESULT {
        if handle.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        let mut st = state(&*handle);
        let Some(r) = st.results.pop_front() else {
            return XAPU_NO_RESULTS;
        };
        let n = r.id.stream as usize;
        st.streams[n].queued -= 1;
        st.dequeued = st.dequeued.wrapping_add(1);
        out.write_unaligned(r);
        S_OK
    }
    // (handle, &total) -> results waiting; `total` receives the results dequeued so far.
    Native fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
        if handle.is_null() {
            return 0;
        }
        let st = state(&*handle);
        if !total.is_null() {
            total.write(st.dequeued);
        }
        st.results.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;
    use std::ptr::{null, null_mut};

    unsafe fn connect(
        streams: u32,
        per_stream: u32,
        options: u32,
    ) -> (*mut Connection, ConnectOutput) {
        let input = ConnectInput {
            process: 1,
            streams,
            commands_per_stream: per_stream,
            data_bytes: 0x10000,
            frames_per_output: 960,
            options,
        };
        let mut out = ConnectOutput {
            data: null_mut(),
            ready: 0,
        };
        let mut h = null_mut();
        assert_eq!(XApuConnect(&input, &mut out, &mut h), S_OK);
        (h, out)
    }

    fn id(kind: u32, stream: u32, options: u32) -> CommandId {
        CommandId {
            kind,
            stream,
            sequence: 0,
            options,
            context: 0,
        }
    }

    unsafe fn dequeue(h: *mut Connection) -> Result<XApuResult, HRESULT> {
        let mut r = std::mem::MaybeUninit::<XApuResult>::zeroed();
        match XApuDequeueResult(h, r.as_mut_ptr()) {
            S_OK => Ok(r.assume_init()),
            e => Err(e),
        }
    }

    #[test]
    fn exports_resolve_by_file_name() {
        for n in [
            "XApuConnect",
            "XApuDequeueResult",
            "XApuDisconnect",
            "XApuEnqueueCommand",
            "XApuGetResultCount",
        ] {
            assert!(lookup("xapu.dll", n).is_some(), "{n}");
        }
    }

    #[test]
    fn connect_checks_arguments_and_returns_memory() {
        unsafe {
            let input = ConnectInput {
                process: 1,
                streams: 0,
                commands_per_stream: 4,
                data_bytes: 0,
                frames_per_output: 0,
                options: 0,
            };
            let mut out = ConnectOutput {
                data: null_mut(),
                ready: 0,
            };
            let mut h = null_mut();
            assert_eq!(XApuConnect(null(), &mut out, &mut h), E_INVALIDARG);
            assert_eq!(XApuConnect(&input, null_mut(), &mut h), E_INVALIDARG);
            assert_eq!(XApuConnect(&input, &mut out, null_mut()), E_INVALIDARG);
            assert_eq!(XApuConnect(&input, &mut out, &mut h), E_INVALIDARG);
            let (h, out) = connect(2, 4, CONNECT_EVENT);
            assert!(!{ out.data }.is_null());
            assert_eq!({ out.data } as usize % 0x1_0000, 0);
            assert_ne!({ out.ready }, 0);
            assert_eq!(XApuDisconnect(h), S_OK);
            assert_eq!(XApuDisconnect(null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn no_results_error_and_counts() {
        unsafe {
            let (h, _) = connect(1, 4, 0);
            assert_eq!(dequeue(h).err(), Some(XAPU_NO_RESULTS));
            assert_eq!(XApuGetResultCount(h, null_mut()), 0);
            assert_eq!(XApuGetResultCount(null_mut(), null_mut()), 0);
            assert_eq!(XApuDequeueResult(null_mut(), null_mut()), E_INVALIDARG);
            let mut total = 99;
            for expect in 1..=2 {
                assert_eq!(XApuEnqueueCommand(h, &id(NOTIFY, 0, 0), &mut total), S_OK);
                assert_eq!(total, expect);
            }
            let mut dequeued = 99;
            assert_eq!(XApuGetResultCount(h, &mut dequeued), 2);
            assert_eq!(dequeued, 0);
            let r = dequeue(h).unwrap();
            assert_eq!({ r.id.kind }, NOTIFY);
            assert_eq!({ r.error }, S_OK);
            assert_eq!(XApuGetResultCount(h, &mut dequeued), 1);
            assert_eq!(dequeued, 1);
            assert!(dequeue(h).is_ok());
            assert_eq!(dequeue(h).err(), Some(XAPU_NO_RESULTS));
            assert_eq!(XApuDisconnect(h), S_OK);
        }
    }

    #[test]
    fn commands_are_checked() {
        unsafe {
            let (h, _) = connect(2, 2, 0);
            assert_eq!(
                XApuEnqueueCommand(null_mut(), &id(NOTIFY, 0, 0), null_mut()),
                E_INVALIDARG
            );
            assert_eq!(XApuEnqueueCommand(h, null(), null_mut()), E_INVALIDARG);
            assert_eq!(
                XApuEnqueueCommand(h, &id(NOTIFY, 2, 0), null_mut()),
                XAPU_INVALID_STREAM
            );
            assert_eq!(
                XApuEnqueueCommand(h, &id(77, 0, 0), null_mut()),
                XAPU_INVALID_COMMAND
            );
            // the queue of a stream is full until a result is dequeued
            assert_eq!(XApuEnqueueCommand(h, &id(NOTIFY, 1, 0), null_mut()), S_OK);
            assert_eq!(XApuEnqueueCommand(h, &id(RESET, 1, 0), null_mut()), S_OK);
            assert_eq!(
                XApuEnqueueCommand(h, &id(NOTIFY, 1, 0), null_mut()),
                XAPU_QUEUE_FULL
            );
            assert_eq!(XApuEnqueueCommand(h, &id(NOTIFY, 0, 0), null_mut()), S_OK);
            assert!(dequeue(h).is_ok());
            assert_eq!(XApuEnqueueCommand(h, &id(NOTIFY, 1, 0), null_mut()), S_OK);
            // a bad channel count is reported in the result
            let bad = Activate {
                id: id(ACTIVATE, 0, 0),
                channels: 3,
            };
            assert_eq!(XApuEnqueueCommand(h, &bad.id, null_mut()), S_OK);
            while let Ok(r) = dequeue(h) {
                if { r.id.kind } == ACTIVATE {
                    assert_eq!({ r.error }, E_INVALIDARG);
                }
            }
            assert_eq!(XApuDisconnect(h), S_OK);
        }
    }

    #[test]
    fn process_answers_silence_of_the_length_asked_for() {
        unsafe {
            let (h, out) = connect(1, 8, CONNECT_EVENT);
            let mut buf = vec![0xAAu8; 3840];
            let bp = buf.as_mut_ptr();
            let process = |frames, output_bytes, options| Process {
                id: id(PROCESS, 0, options),
                ramp: 0.0,
                pitch: 48000.0,
                first_frame: 312,
                frames,
                input_bytes: 80,
                input: out.data,
                output_bytes,
                output: bp.cast(),
            };
            // not active yet
            let p = process(960, 3840, 0);
            assert_eq!(XApuEnqueueCommand(h, &p.id, null_mut()), S_OK);
            assert_eq!({ dequeue(h).unwrap().error }, XAPU_INVALID_STREAM);
            let act = Activate {
                id: id(ACTIVATE, 0, 0),
                channels: 1,
            };
            assert_eq!(XApuEnqueueCommand(h, &act.id, null_mut()), S_OK);
            assert_eq!({ dequeue(h).unwrap().error }, S_OK);
            let p = process(960, 3840, OPT_SIGNAL);
            assert_eq!(XApuEnqueueCommand(h, &p.id, null_mut()), S_OK);
            assert_eq!(handles::wait(&[out.ready], false, 0), 0, "event signalled");
            let r = dequeue(h).unwrap();
            assert_eq!({ r.state }, STATE_READY | STATE_WANT_INPUT);
            assert_eq!({ r.frames }, 960);
            assert_eq!({ r.output_bytes }, 3840);
            assert_eq!({ r.output }, bp.cast());
            assert_eq!({ r.input_bytes }, 80);
            assert_eq!({ r.pitch }, 48000.0);
            assert!(buf.iter().all(|&b| b == 0));
            // an output buffer shorter than the frames asked for limits the frames
            buf.fill(0xAA);
            let p = process(960, 400, 0);
            assert_eq!(XApuEnqueueCommand(h, &p.id, null_mut()), S_OK);
            let r = dequeue(h).unwrap();
            assert_eq!({ r.frames }, 100);
            assert!(buf[..400].iter().all(|&b| b == 0) && buf[400] == 0xAA);
            // stereo takes twice the bytes per frame; frames of 0 mean the connection's 960
            let act = Activate {
                id: id(ACTIVATE, 0, 0),
                channels: 2,
            };
            assert_eq!(XApuEnqueueCommand(h, &act.id, null_mut()), S_OK);
            let p = process(0, 3840, 0);
            assert_eq!(XApuEnqueueCommand(h, &p.id, null_mut()), S_OK);
            dequeue(h).unwrap();
            let r = dequeue(h).unwrap();
            assert_eq!({ r.frames }, 480);
            assert_eq!({ r.output_bytes }, 3840);
            let deact = id(DEACTIVATE, 0, 0);
            assert_eq!(XApuEnqueueCommand(h, &deact, null_mut()), S_OK);
            assert_eq!(XApuDisconnect(h), S_OK);
        }
    }
}
