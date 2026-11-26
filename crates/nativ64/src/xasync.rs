//! The game runtime's task queues and async-call blocks (`XTaskQueue*`, `XAsync*`): the minimum
//! the user path needs, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! What exists:
//! - Queues: a work port and a completion port, each in one of four modes (0 manual, dispatched by
//!   the title with `XTaskQueueDispatch`; 1 thread pool; 2 serialized; 3 immediate). Thread-pool
//!   and serialized ports start one short-lived thread per task (serialized tasks take a lock, so
//!   they never overlap, but their order is not guaranteed). A NULL queue handle is the process
//!   queue, a thread-pool queue made on first use. Handles are `Arc::into_raw`; duplicating and
//!   closing add and drop a reference.
//! - Async calls: `XAsyncBegin` files a call by the address of the title's block, its provider
//!   runs `Begin`, `DoWork`, `GetResult`, `Cancel` and `Cleanup`, and `XAsyncComplete` publishes
//!   the status and result size in the block's system area, wakes waiters and queues the block's
//!   completion callback on the completion port. `XAsyncGetResult` fetches the result from the
//!   provider and ends the call; a call that fails or has no result ends at completion.
//!   [`finished`] starts a call that is already over (what most host methods need).
//!
//! - Ports: `XTaskQueueGetPort` hands out a port (a reference we never take back) and a composite
//!   queue is made from a work port and a completion port.
//!
//! Not done (traps log the slot and answer `E_NOTIMPL`): termination, monitors, wait handles,
//! `XAsyncRun`, thread time-sensitivity. No Wine oracle exists.
//! The slot order is that of the public GDK header as read through another loader's table.
#![allow(clippy::not_unsafe_ptr_arg_deref, non_snake_case)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::com::Guid;
use crate::xgameruntime::{new_object, slot_trap, table, Table, AREA_QUEUE, S_OK};

pub type HRESULT = i32;
pub const E_PENDING: HRESULT = 0x8000_000Au32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007Au32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000Eu32 as i32;

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid::new(
    0x073b_7dcb,
    0x1fcf,
    0x4030,
    [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
);
static IIDS: [Guid; 1] = [PROVIDER];

const WORK: u32 = 0;
const COMPLETION: u32 = 1;

/// Callback of a queued task: (context, canceled).
type Callback = extern "win64" fn(*mut c_void, bool);
/// Completion routine of an async block.
type Routine = extern "win64" fn(*mut AsyncBlock);
/// An async provider: (operation, data).
pub type Provider = extern "win64" fn(u32, *const ProviderData) -> HRESULT;

pub const OP_BEGIN: u32 = 0;
pub const OP_DO_WORK: u32 = 1;
pub const OP_GET_RESULT: u32 = 2;
pub const OP_CANCEL: u32 = 3;
pub const OP_CLEANUP: u32 = 4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(raw: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(raw as usize)
        .copied()
    }
}

/// A title callback and its context, as addresses so tasks cross threads.
#[derive(Clone, Copy)]
struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
        }
    }
    fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        callback(self.context as *mut c_void, canceled);
    }
}

pub struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
    serial: Mutex<()>,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
            serial: Mutex::new(()),
        })
    }

    fn submit(self: &Arc<Self>, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => {
                std::thread::spawn(move || task.run(false));
            }
            Mode::Serialized => {
                let port = self.clone();
                std::thread::spawn(move || {
                    let _one_at_a_time = lock(&port.serial);
                    task.run(false);
                });
            }
            Mode::Manual => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// Runs one queued task of a manual port, waiting up to `timeout_ms`; true if one ran.
    fn dispatch(&self, timeout_ms: u32) -> bool {
        if self.mode != Mode::Manual {
            return false;
        }
        let tasks = lock(&self.tasks);
        let wait = |t: &mut VecDeque<Task>| t.is_empty();
        let mut tasks = if timeout_ms == u32::MAX {
            self.ready
                .wait_while(tasks, wait)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            self.ready
                .wait_timeout_while(tasks, Duration::from_millis(timeout_ms.into()), wait)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        let task = tasks.pop_front();
        drop(tasks);
        match task {
            Some(task) => {
                task.run(false);
                true
            }
            None => false,
        }
    }
}

/// A task queue; a handle is `Arc::into_raw` of it.
pub struct Queue {
    ports: [Arc<Port>; 2],
}

impl Queue {
    fn new(work: Mode, completion: Mode) -> Arc<Queue> {
        Arc::new(Queue {
            ports: [Port::new(work), Port::new(completion)],
        })
    }

    fn submit(&self, port: u32, delay_ms: u32, task: Task) -> HRESULT {
        let Some(port) = self.ports.get(port as usize).cloned() else {
            return E_INVALIDARG;
        };
        if delay_ms == 0 {
            port.submit(task);
        } else {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay_ms.into()));
                port.submit(task);
            });
        }
        S_OK
    }
}

/// The process queue, made (thread pool) when first used.
static PROCESS_QUEUE: Mutex<Option<Arc<Queue>>> = Mutex::new(None);

/// A new reference to the queue behind `handle` (NULL: the process queue).
fn queue(handle: *const Queue) -> Arc<Queue> {
    if handle.is_null() {
        return lock(&PROCESS_QUEUE)
            .get_or_insert_with(|| Queue::new(Mode::ThreadPool, Mode::ThreadPool))
            .clone();
    }
    // SAFETY: a handle we returned and the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Arc::from_raw(handle)
    }
}

/// An async block: what the title fills in, then the system area (ours:
/// [unused, status, result size, unused]).
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<Routine>,
    pub internal: [usize; 4],
}

/// What a provider is called with.
#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

/// One call in flight, until its result is taken or it fails.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<bool>,
    finished: Condvar,
}

impl Call {
    fn provide(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }
}

/// Calls by block address.
static CALLS: Mutex<Option<HashMap<usize, Arc<Call>>>> = Mutex::new(None);

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).as_ref()?.get(&(block as usize)).cloned()
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Ends the call on `block`: removed, its provider cleans up.
fn release(block: *mut AsyncBlock) {
    let removed = lock(&CALLS)
        .as_mut()
        .and_then(|c| c.remove(&(block as usize)));
    if let Some(call) = removed {
        call.provide(OP_CLEANUP, block, 0, std::ptr::null_mut());
    }
}

pub extern "win64" fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() {
        return E_INVALIDARG;
    }
    if let Some(old) = call(block) {
        // A finished call whose result was never taken is replaced; one in flight is busy.
        if !*lock(&old.done) {
            return E_INVALIDARG;
        }
        release(block);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::new(false),
        finished: Condvar::new(),
    });
    lock(&CALLS)
        .get_or_insert_with(HashMap::new)
        .insert(block as usize, call.clone());
    let hr = call.provide(OP_BEGIN, block, 0, std::ptr::null_mut());
    if hr < 0 {
        XAsyncComplete(block, hr, 0);
    }
    S_OK
}

/// Queues the provider's `DoWork` on the block's work port.
pub extern "win64" fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    extern "win64" fn work(context: *mut c_void, canceled: bool) {
        // SAFETY: boxed in `XAsyncSchedule`; a task runs once.
        let block = unsafe { *Box::from_raw(context.cast::<usize>()) } as *mut AsyncBlock;
        let Some(call) = call(block) else { return };
        let hr = if canceled {
            E_ABORT
        } else {
            call.provide(OP_DO_WORK, block, 0, std::ptr::null_mut())
        };
        XAsyncComplete(block, hr, 0);
    }
    if block.is_null() || call(block).is_none() {
        return E_INVALIDARG;
    }
    // SAFETY: a block in flight is valid.
    let queue = queue(unsafe { (*block).queue });
    let context = Box::into_raw(Box::new(block as usize));
    let hr = queue.submit(WORK, delay_ms, Task::new(context.cast(), work));
    if hr < 0 {
        // SAFETY: refused, so the task never runs and the box is still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Records the result (the first completion wins), wakes waiters and queues the block's
/// completion routine on its completion port.
pub extern "win64" fn XAsyncComplete(
    block: *mut AsyncBlock,
    result: HRESULT,
    required_size: usize,
) {
    extern "win64" fn complete(block: *mut c_void, _canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        // SAFETY: read before the status was published; the title keeps the block until then.
        if let Some(routine) = unsafe { (*block).callback } {
            routine(block);
        }
    }
    if result == E_PENDING || block.is_null() {
        return;
    }
    let Some(call) = call(block) else { return };
    {
        let mut done = lock(&call.done);
        if *done {
            return;
        }
        *done = true;
    }
    // Read before publishing: once the status is set, the title may free the block.
    // SAFETY: a block in flight is valid.
    let (handle, routine) = unsafe { ((*block).queue, (*block).callback) };
    let (status, size) = words(block);
    if result < 0 || required_size == 0 {
        release(block);
    }
    size.store(required_size, Ordering::Release);
    status.store(result as u32 as usize, Ordering::Release);
    call.finished.notify_all();
    if routine.is_some() {
        let task = Task::new(block.cast(), complete);
        if queue(handle).submit(COMPLETION, 0, task) != S_OK {
            task.run(true);
        }
    }
}

pub extern "win64" fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait {
        if let Some(call) = call(block) {
            let done = lock(&call.done);
            drop(call.finished.wait_while(done, |d| !*d));
        }
    }
    status(block)
}

pub extern "win64" fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless it completed first.
pub extern "win64" fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block) {
        if !*lock(&call.done) {
            call.provide(OP_CANCEL, block, 0, std::ptr::null_mut());
            XAsyncComplete(block, E_ABORT, 0);
        }
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub extern "win64" fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let hr = call.provide(OP_GET_RESULT, block, buffer_size, buffer);
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr` and, when `hr` is a
/// success, a result of `result.len()` bytes that `XAsyncGetResult` for `identity` copies out.
pub fn finished(
    block: *mut AsyncBlock,
    identity: *const c_void,
    hr: HRESULT,
    result: Vec<u8>,
) -> HRESULT {
    type Data = (HRESULT, Vec<u8>);
    extern "win64" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: valid provider data; the context is the boxed result until cleanup.
        let data = unsafe { &*data };
        let state = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(state) }),
            OP_BEGIN => {
                // Copied out: completing may clean up.
                // SAFETY: as above.
                let (hr, len) = unsafe { ((*state).0, (*state).1.len()) };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                let bytes = &(*state).1;
                data.buffer
                    .cast::<u8>()
                    .copy_from(bytes.as_ptr(), bytes.len());
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new((hr, result)));
    let started = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if started < 0 {
        // SAFETY: refused before the call existed, so no cleanup will come.
        drop(unsafe { Box::from_raw(context) });
    }
    started
}

// Queue functions, as the provider object's methods.

pub extern "win64" fn XTaskQueueCreate(
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(Queue::new(work, completion))) };
    S_OK
}

/// The work (0) or completion (1) port of a queue, as a handle we never reclaim.
pub extern "win64" fn XTaskQueueGetPort(
    handle: *const Queue,
    port: u32,
    out: *mut *const Port,
) -> HRESULT {
    if out.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    let Some(port) = queue(handle).ports.get(port as usize).cloned() else {
        return E_INVALIDARG;
    };
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(port)) };
    S_OK
}

/// A queue made of the work port of one queue and the completion port of another.
pub extern "win64" fn XTaskQueueCreateComposite(
    work: *const Port,
    completion: *const Port,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: handles from `XTaskQueueGetPort`, whose references are never dropped.
    let (work, completion) = unsafe {
        Arc::increment_strong_count(work);
        Arc::increment_strong_count(completion);
        (Arc::from_raw(work), Arc::from_raw(completion))
    };
    let composite = Arc::new(Queue {
        ports: [work, completion],
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(composite)) };
    S_OK
}

pub extern "win64" fn XTaskQueueDuplicateHandle(
    handle: *const Queue,
    out: *mut *const Queue,
) -> HRESULT {
    if handle.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a handle we returned; the new reference is the copy's.
    unsafe { out.write(Arc::into_raw(queue(handle))) };
    S_OK
}

pub extern "win64" fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops the title's reference.
        drop(unsafe { Arc::from_raw(handle) });
    }
}

/// Runs one callback of a manual port, waiting up to `timeout_ms`; true if one ran.
pub extern "win64" fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let queue = queue(handle);
    queue
        .ports
        .get(port as usize)
        .is_some_and(|p| p.dispatch(timeout_ms))
}

pub extern "win64" fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    queue(handle).submit(port, delay_ms, Task::new(context, callback))
}

/// Returns the process queue handle (a new reference) through `out`; false if none was set.
pub extern "win64" fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    if out.is_null() {
        return false;
    }
    let set = lock(&PROCESS_QUEUE).clone();
    match set {
        Some(q) => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(q)) };
            true
        }
        None => false,
    }
}

/// Makes `handle` the process queue (NULL clears it, so the next use makes a new one).
pub extern "win64" fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    *lock(&PROCESS_QUEUE) = (!handle.is_null()).then(|| queue(handle));
}

type This = *mut c_void;
extern "win64" fn m_get_status(_: This, b: *mut AsyncBlock, wait: u8) -> HRESULT {
    XAsyncGetStatus(b, wait != 0)
}
extern "win64" fn m_get_result_size(_: This, b: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, out)
}
extern "win64" fn m_cancel(_: This, b: *mut AsyncBlock) {
    XAsyncCancel(b)
}
extern "win64" fn m_begin(
    _: This,
    b: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    XAsyncBegin(b, context, identity, name, provider)
}
extern "win64" fn m_schedule(_: This, b: *mut AsyncBlock, delay: u32) -> HRESULT {
    XAsyncSchedule(b, delay)
}
extern "win64" fn m_complete(_: This, b: *mut AsyncBlock, hr: HRESULT, size: usize) {
    XAsyncComplete(b, hr, size)
}
extern "win64" fn m_get_result(
    _: This,
    b: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    XAsyncGetResult(b, identity, size, buffer, used)
}
extern "win64" fn m_create(_: This, work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    XTaskQueueCreate(work, completion, out)
}
extern "win64" fn m_create_composite(
    _: This,
    work: *const Port,
    completion: *const Port,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreateComposite(work, completion, out)
}
extern "win64" fn m_get_port(
    _: This,
    q: *const Queue,
    port: u32,
    out: *mut *const Port,
) -> HRESULT {
    XTaskQueueGetPort(q, port, out)
}
extern "win64" fn m_duplicate(_: This, q: *const Queue, out: *mut *const Queue) -> HRESULT {
    XTaskQueueDuplicateHandle(q, out)
}
extern "win64" fn m_dispatch(_: This, q: *const Queue, port: u32, timeout: u32) -> bool {
    XTaskQueueDispatch(q, port, timeout)
}
extern "win64" fn m_close(_: This, q: *const Queue) {
    XTaskQueueCloseHandle(q)
}
extern "win64" fn m_submit(
    _: This,
    q: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, 0, context, callback)
}
extern "win64" fn m_submit_delayed(
    _: This,
    q: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, delay, context, callback)
}
extern "win64" fn m_get_process_queue(_: This, out: *mut *const Queue) -> bool {
    XTaskQueueGetCurrentProcessTaskQueue(out)
}
extern "win64" fn m_set_process_queue(_: This, q: *const Queue) {
    XTaskQueueSetCurrentProcessTaskQueue(q)
}
/// The runtime's debug check that the caller is not a time-critical thread; every runtime call
/// of a title makes it first.
extern "win64" fn m_check_thread(_: This) {}

const SLOTS: usize = 31;
const fn t<const S: usize>() -> extern "win64" fn() -> i32 {
    slot_trap::<AREA_QUEUE, S>
}

static VTBL: Table<SLOTS> = table!(SLOTS;
    m_get_status,        // 3 XAsyncGetStatus
    m_get_result_size,   // 4
    m_cancel,            // 5
    t::<6>(),            // XAsyncRun
    m_begin,             // 7
    t::<8>(),            // (unknown)
    m_schedule,          // 9
    m_complete,          // 10
    m_get_result,        // 11
    m_create,            // 12
    m_create_composite,  // 13
    m_get_port,          // 14
    m_duplicate,         // 15
    m_dispatch,          // 16
    m_close,             // 17
    m_submit,            // 18
    m_submit_delayed,    // 19
    t::<20>(),           // register wait handle
    t::<21>(),           // unregister it
    t::<22>(),           // terminate
    t::<23>(),           // register monitor
    t::<24>(),           // unregister monitor
    m_get_process_queue, // 25
    m_set_process_queue, // 26
    t::<27>(),           // thread time-sensitivity
    t::<28>(),
    m_check_thread,      // 29
    t::<30>(),
);

/// The provider's object for `QueryApiImpl`.
pub fn query(api: &Guid, iid: &Guid) -> Option<*mut c_void> {
    (*api == PROVIDER && *iid == PROVIDER).then(|| new_object(&VTBL, &IIDS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{release, ComObject};
    use crate::xgameruntime::method;
    use std::sync::atomic::AtomicU32;

    fn block(queue: *const Queue) -> AsyncBlock {
        AsyncBlock {
            queue,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "win64" fn count(context: *mut c_void, canceled: bool) {
        RAN.fetch_add(
            if canceled { 1000 } else { context as u32 },
            Ordering::SeqCst,
        );
    }

    #[test]
    fn manual_ports_run_callbacks_when_dispatched() {
        unsafe {
            let o = query(&PROVIDER, &PROVIDER).unwrap();
            let create: extern "win64" fn(This, u32, u32, *mut *const Queue) -> HRESULT =
                method(o, 12);
            let submit: extern "win64" fn(
                This,
                *const Queue,
                u32,
                *mut c_void,
                Option<Callback>,
            ) -> HRESULT = method(o, 18);
            let dispatch: extern "win64" fn(This, *const Queue, u32, u32) -> bool = method(o, 16);
            let dup: extern "win64" fn(This, *const Queue, *mut *const Queue) -> HRESULT =
                method(o, 15);
            let close: extern "win64" fn(This, *const Queue) = method(o, 17);
            let mut q = std::ptr::null();
            assert_eq!(create(o, 0, 0, &mut q), S_OK);
            assert_eq!(create(o, 9, 0, &mut q), E_INVALIDARG);
            let before = RAN.load(Ordering::SeqCst);
            assert_eq!(submit(o, q, 0, 7 as *mut c_void, Some(count)), S_OK);
            assert_eq!(
                submit(o, q, 5, std::ptr::null_mut(), Some(count)),
                E_INVALIDARG
            );
            assert_eq!(submit(o, q, 0, std::ptr::null_mut(), None), E_INVALIDARG);
            assert!(!dispatch(o, q, 1, 0), "other port is empty");
            let mut copy = std::ptr::null();
            assert_eq!(dup(o, q, &mut copy), S_OK);
            assert!(dispatch(o, copy, 0, 0));
            assert_eq!(RAN.load(Ordering::SeqCst) - before, 7);
            assert!(!dispatch(o, q, 0, 10), "empty after the timeout");
            close(o, copy);
            close(o, q);
            release(o as *mut ComObject<Table<SLOTS>, ()>);
        }
    }

    #[test]
    fn composite_queues_share_ports() {
        let (mut a, mut b, mut c) = (std::ptr::null(), std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        assert_eq!(XTaskQueueCreate(0, 0, &mut b), S_OK);
        let (mut work, mut done) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(b, COMPLETION, &mut done), S_OK);
        assert_eq!(XTaskQueueGetPort(a, 2, &mut done), E_INVALIDARG);
        assert_eq!(XTaskQueueCreateComposite(work, done, &mut c), S_OK);
        let before = RAN.load(Ordering::SeqCst);
        // Work submitted to the composite is dispatched from a's work port.
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(c, WORK, 0, 3 as *mut c_void, Some(count)),
            S_OK
        );
        assert!(XTaskQueueDispatch(a, WORK, 0));
        assert_eq!(RAN.load(Ordering::SeqCst) - before, 3);
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(c, COMPLETION, 0, 4 as *mut c_void, Some(count)),
            S_OK
        );
        assert!(!XTaskQueueDispatch(a, COMPLETION, 0));
        assert!(XTaskQueueDispatch(b, COMPLETION, 0));
        assert_eq!(RAN.load(Ordering::SeqCst) - before, 7);
        assert_eq!(
            XTaskQueueCreateComposite(std::ptr::null(), done, &mut c),
            E_INVALIDARG
        );
        for q in [a, b, c] {
            XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn immediate_and_thread_pool_ports_run_by_themselves() {
        let before = RAN.load(Ordering::SeqCst);
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(3, 3, &mut q), S_OK);
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, 0, 0, 20 as *mut c_void, Some(count)),
            S_OK
        );
        assert_eq!(RAN.load(Ordering::SeqCst) - before, 20);
        XTaskQueueCloseHandle(q);
        let done = Arc::new(AtomicU32::new(0));
        let q = Queue::new(Mode::ThreadPool, Mode::Serialized);
        for (port, delay) in [(0, 0), (1, 5)] {
            let flag = Arc::into_raw(done.clone()) as *mut c_void;
            extern "win64" fn bump(context: *mut c_void, _: bool) {
                // SAFETY: made by `Arc::into_raw` above; each task runs once.
                unsafe { Arc::from_raw(context as *const AtomicU32) }
                    .fetch_add(1, Ordering::SeqCst);
            }
            assert_eq!(q.submit(port, delay, Task::new(flag, bump)), S_OK);
        }
        for _ in 0..200 {
            if done.load(Ordering::SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn process_queue_is_created_on_use_and_can_be_set() {
        let mut out = std::ptr::null();
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        XTaskQueueSetCurrentProcessTaskQueue(q);
        assert!(XTaskQueueGetCurrentProcessTaskQueue(&mut out));
        assert_eq!(out, q);
        XTaskQueueCloseHandle(out);
        XTaskQueueSetCurrentProcessTaskQueue(std::ptr::null());
        assert!(!XTaskQueueGetCurrentProcessTaskQueue(&mut out));
        XTaskQueueCloseHandle(q);
    }

    static IDENTITY: u8 = 0;
    extern "win64" fn two_step(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_OK, 4);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: the runtime checked the buffer's size.
                unsafe { data.buffer.cast::<u32>().write(0xfeed) };
                S_OK
            }
            _ => S_OK,
        }
    }

    #[test]
    fn scheduled_call_runs_on_the_work_port_and_gives_its_result() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut b = block(q);
        let id = (&raw const IDENTITY).cast();
        assert_eq!(
            XAsyncBegin(
                &mut b,
                std::ptr::null_mut(),
                id,
                std::ptr::null(),
                Some(two_step)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut b, false), E_PENDING);
        // Busy: a second begin on the same block is refused.
        assert_eq!(
            XAsyncBegin(
                &mut b,
                std::ptr::null_mut(),
                id,
                std::ptr::null(),
                Some(two_step)
            ),
            E_INVALIDARG
        );
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut b, &mut size), E_PENDING);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(XAsyncGetResultSize(&mut b, &mut size), S_OK);
        assert_eq!(size, 4);
        let mut v = 0u32;
        let mut used = 0;
        let buf = (&raw mut v).cast();
        assert_eq!(
            XAsyncGetResult(&mut b, std::ptr::null(), 4, buf, &mut used),
            E_INVALIDARG
        );
        assert_eq!(
            XAsyncGetResult(&mut b, id, 2, buf, &mut used),
            E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(XAsyncGetResult(&mut b, id, 4, buf, &mut used), S_OK);
        assert_eq!((v, used), (0xfeed, 4));
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn cancel_fails_a_pending_call_and_a_failed_begin_completes() {
        extern "win64" fn never(op: u32, _: *const ProviderData) -> HRESULT {
            if op == OP_BEGIN {
                E_NOT_SUFFICIENT_BUFFER
            } else {
                S_OK
            }
        }
        extern "win64" fn waits(_: u32, _: *const ProviderData) -> HRESULT {
            S_OK
        }
        let id = (&raw const IDENTITY).cast();
        let np = std::ptr::null_mut();
        let mut b = block(std::ptr::null());
        assert_eq!(
            XAsyncBegin(&mut b, np, id, std::ptr::null(), Some(never)),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut b, true), E_NOT_SUFFICIENT_BUFFER);
        let mut c = block(std::ptr::null());
        assert_eq!(
            XAsyncBegin(&mut c, np, id, std::ptr::null(), Some(waits)),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut c, false), E_PENDING);
        XAsyncCancel(&mut c);
        assert_eq!(XAsyncGetStatus(&mut c, true), E_ABORT);
        assert_eq!(
            XAsyncBegin(&mut c, np, id, std::ptr::null(), None),
            E_INVALIDARG
        );
        assert_eq!(XAsyncGetStatus(std::ptr::null_mut(), false), E_INVALIDARG);
    }

    #[test]
    fn the_provider_answers_only_its_own_id() {
        assert!(query(&PROVIDER, &PROVIDER).is_some());
        assert!(query(&PROVIDER, &crate::xuser::IID).is_none());
    }
}
