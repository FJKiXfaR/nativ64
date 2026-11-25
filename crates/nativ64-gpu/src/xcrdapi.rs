//! `XCRDAPI.dll`: the console's DirectStorage. One export, `DStorageGetFactory`, returns the
//! factory; files, queues and status arrays hang off it as COM objects (`nativ64::com`).
//!
//! Slot orders and the request layout are the console's (they differ from the desktop SDK in
//! places). Factory: 3 (unused here), 4 OpenFile, 5 CreateStatusArray, 6 (CPU affinity, unused),
//! 7 SetDebugFlags, 8 SetStagingBufferSize, 9 (unused), 10 CreateQueue, 11..15 unused. File: 3
//! Close, 4 GetFileInformation. Status array: 3 IsComplete, 4 GetHResult. Queue: 3
//! EnqueueRequest, 4 EnqueueStatus, 5 EnqueueSignal, 6 Submit, 7 CancelRequestsWithTag, 8 Close,
//! 9 GetErrorEvent, 10 RetrieveErrorRecord, 11 Query, 12 EnqueueSetEvent (slot 12 follows the
//! desktop SDK's later interface; unconfirmed on the console).
//!
//! Behaviour: `Submit` runs the queued commands in order on the calling thread. A read copies a
//! memory source or `pread`s a file into the destination address; a status entry gets the first
//! failure since the previous entry (or `S_OK`) and is pending from its enqueue; an event
//! command sets the event. A failed read also goes in the queue's error record and signals the
//! queue's error event.
//!
//! Not done (TODO, all fail the request with `E_NOTIMPL`): zlib and BCPack decompression
//! (options bits 0..4), the further transforms (bits 5..20), page destinations (bit 21), physical
//! page sources (bit 23), and GPU buffer/texture destinations. Fences: `EnqueueSignal` records
//! the fence and value ([`signals`]) but signals nothing, as no `ID3D12Fence` exists yet. The
//! error record is written as failure count, first failure HRESULT, command type, then the
//! request name (64 bytes) and a copy of the request; the real layout is unconfirmed.
//! Object IIDs other than the factory's are unknown: creation honours the IID asked for.

use nativ64::com::{
    release, ComObject, Guid, IUnknownVtbl, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER,
    HRESULT, S_OK,
};
use nativ64::handles::{close_handle, create_event, reset_event, set_event, Handle};
use std::ffi::{c_char, c_void, CStr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

pub const E_PENDING: HRESULT = 0x8000_000Au32 as i32;
const ERROR_FILE_NOT_FOUND: HRESULT = 0x8007_0002u32 as i32;
const ERROR_PATH_NOT_FOUND: HRESULT = 0x8007_0003u32 as i32;
const ERROR_ACCESS_DENIED: HRESULT = 0x8007_0005u32 as i32;
const ERROR_GEN_FAILURE: HRESULT = 0x8007_001Fu32 as i32;
const ERROR_HANDLE_EOF: HRESULT = 0x8007_0026u32 as i32;
const ERROR_INSUFFICIENT_BUFFER: HRESULT = 0x8007_007Au32 as i32;
const ERROR_INVALID_HANDLE: HRESULT = 0x8007_0006u32 as i32;

/// `{62959d11-d45f-4884-a0a3-f7862ea243ae}`: the console's factory interface.
pub const IID_FACTORY_X: Guid = Guid::new(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);
/// `{6924ea0c-c3cd-4826-b10a-f64f4ed927c1}`: the desktop `IDStorageFactory`.
pub const IID_FACTORY: Guid = Guid::new(
    0x6924_ea0c,
    0xc3cd,
    0x4826,
    [0xb1, 0x0a, 0xf6, 0x4f, 0x4e, 0xd9, 0x27, 0xc1],
);
static FACTORY_IIDS: [Guid; 2] = [IID_FACTORY_X, IID_FACTORY];

// Request options (the console's layout).
const OPT_COMPRESSION: u64 = 0x1f; // zlib (bit 0), BCPack format (bits 1..4)
const OPT_TRANSFORMS: u64 = 0xffff << 5;
const OPT_PAGE_DESTINATION: u64 = 1 << 21;
const OPT_MEMORY_SOURCE: u64 = 1 << 22;
const OPT_PHYSICAL_SOURCE: u64 = 1 << 23;

/// `DSTORAGE_REQUEST` as the console's title builds it (0x48 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Request {
    pub options: u64,
    /// A destination address (page numbers with `OPT_PAGE_DESTINATION`).
    pub destination: *mut c_void,
    pub page_offset: u64,
    pub destination_size: u32,
    /// A file object, or with `OPT_MEMORY_SOURCE` an address.
    pub source: *const c_void,
    pub offset: u64,
    pub source_size: u32,
    pub intermediate_size: u32,
    /// Matched by `CancelRequestsWithTag`.
    pub tag: u64,
    pub name: *const c_char,
}

/// The queue description (`DSTORAGE_QUEUE_DESC`, console layout).
#[repr(C)]
pub struct QueueDesc {
    pub source_type: u64,
    pub capacity: u16,
    pub priority: i8,
    pub name: *const c_char,
    pub unknown: u64,
}

// ------------------------------------------------------------------ HRESULT helper

fn from_errno(e: i32) -> HRESULT {
    match e {
        libc::ENOENT => ERROR_FILE_NOT_FOUND,
        libc::ENOTDIR => ERROR_PATH_NOT_FOUND,
        libc::EACCES | libc::EPERM => ERROR_ACCESS_DENIED,
        libc::EBADF => ERROR_INVALID_HANDLE,
        _ => ERROR_GEN_FAILURE,
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ------------------------------------------------------------------ status array

type StatusObj = ComObject<StatusVtbl, Vec<AtomicI32>>;

#[repr(C)]
pub struct StatusVtbl {
    unknown: IUnknownVtbl<StatusVtbl, Vec<AtomicI32>>,
    is_complete: unsafe extern "win64" fn(*mut StatusObj, u32) -> i32,
    get_hresult: unsafe extern "win64" fn(*mut StatusObj, u32) -> HRESULT,
}

unsafe extern "win64" fn status_hresult(this: *mut StatusObj, i: u32) -> HRESULT {
    let entries: &Vec<AtomicI32> = &(*this).data;
    entries
        .get(i as usize)
        .map_or(E_INVALIDARG, |e| e.load(Ordering::Acquire))
}

unsafe extern "win64" fn status_is_complete(this: *mut StatusObj, i: u32) -> i32 {
    i32::from(status_hresult(this, i) != E_PENDING)
}

static STATUS_VTBL: StatusVtbl = StatusVtbl {
    unknown: IUnknownVtbl::new(),
    is_complete: status_is_complete,
    get_hresult: status_hresult,
};

// ------------------------------------------------------------------ file

/// The descriptor, `-1` once closed.
pub struct FileState {
    fd: AtomicI32,
}

impl Drop for FileState {
    fn drop(&mut self) {
        let fd = self.fd.swap(-1, Ordering::AcqRel);
        if fd >= 0 {
            // SAFETY: we own the descriptor.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
}

type FileObj = ComObject<FileVtbl, FileState>;

#[repr(C)]
pub struct FileVtbl {
    unknown: IUnknownVtbl<FileVtbl, FileState>,
    close: unsafe extern "win64" fn(*mut FileObj),
    get_file_information: unsafe extern "win64" fn(*mut FileObj, *mut u8) -> HRESULT,
}

unsafe extern "win64" fn file_close(this: *mut FileObj) {
    let fd = (*this).data.fd.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        drop(OwnedFd::from_raw_fd(fd));
    }
}

/// Fills a `BY_HANDLE_FILE_INFORMATION` (52 bytes): normal attributes, the size, one link.
unsafe extern "win64" fn file_information(this: *mut FileObj, out: *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let fd = (*this).data.fd.load(Ordering::Acquire);
    let mut st: libc::stat = std::mem::zeroed();
    if fd < 0 || libc::fstat(fd, &mut st) != 0 {
        return ERROR_INVALID_HANDLE;
    }
    let mut info = [0u8; 52];
    info[0..4].copy_from_slice(&0x80u32.to_le_bytes());
    let size = st.st_size as u64;
    info[36..40].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    info[40..44].copy_from_slice(&(size as u32).to_le_bytes());
    info[44..48].copy_from_slice(&1u32.to_le_bytes());
    std::ptr::copy_nonoverlapping(info.as_ptr(), out, info.len());
    S_OK
}

static FILE_VTBL: FileVtbl = FileVtbl {
    unknown: IUnknownVtbl::new(),
    close: file_close,
    get_file_information: file_information,
};

// ------------------------------------------------------------------ queue

enum Command {
    Read(Request, String),
    Status(*mut StatusObj, u32),
    Signal(usize, u64),
    Event(Handle),
}
// SAFETY: the pointers name title memory and our own objects, valid until the title has seen the
// commands complete.
unsafe impl Send for Command {}

/// The first failure of a queue since the record was last retrieved.
#[derive(Clone)]
struct Failure {
    hresult: HRESULT,
    command_type: u32,
    name: String,
    request: Option<Request>,
}
unsafe impl Send for Failure {}

#[derive(Default)]
struct QueueInner {
    pending: Vec<Command>,
    closed: bool,
    failures: u32,
    first: Option<Failure>,
    signals: Vec<(usize, u64)>,
}

#[derive(Default)]
pub struct QueueState {
    inner: Mutex<QueueInner>,
    error_event: AtomicUsize,
}

impl Drop for QueueState {
    fn drop(&mut self) {
        let h = *self.error_event.get_mut();
        if h != 0 {
            close_handle(h);
        }
    }
}

type QueueObj = ComObject<QueueVtbl, QueueState>;
type Q = *mut QueueObj;

#[repr(C)]
pub struct QueueVtbl {
    unknown: IUnknownVtbl<QueueVtbl, QueueState>,
    enqueue_request: unsafe extern "win64" fn(Q, *const Request),
    enqueue_status: unsafe extern "win64" fn(Q, *mut StatusObj, u32),
    enqueue_signal: unsafe extern "win64" fn(Q, *mut c_void, u64),
    submit: unsafe extern "win64" fn(Q),
    cancel_requests_with_tag: unsafe extern "win64" fn(Q, u64, u64),
    close: unsafe extern "win64" fn(Q),
    get_error_event: unsafe extern "win64" fn(Q) -> Handle,
    retrieve_error_record: unsafe extern "win64" fn(Q, *mut u8),
    query: unsafe extern "win64" fn(Q, *mut c_void),
    enqueue_set_event: unsafe extern "win64" fn(Q, Handle),
}

fn lock(q: &QueueState) -> std::sync::MutexGuard<'_, QueueInner> {
    q.inner.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn push(q: Q, c: Command) {
    let mut g = lock(&(*q).data);
    if g.closed {
        if let Command::Status(a, _) = c {
            release::<StatusVtbl, Vec<AtomicI32>>(a);
        }
        return;
    }
    g.pending.push(c);
}

unsafe extern "win64" fn q_enqueue_request(q: Q, r: *const Request) {
    if let Some(r) = r.as_ref() {
        let name = text(r.name);
        push(q, Command::Read(*r, name));
    }
}

unsafe extern "win64" fn q_enqueue_status(q: Q, a: *mut StatusObj, index: u32) {
    let Some(entry) = a.as_ref().and_then(|a| a.data.get(index as usize)) else {
        return;
    };
    entry.store(E_PENDING, Ordering::Release);
    nativ64::com::add_ref::<StatusVtbl, Vec<AtomicI32>>(a);
    push(q, Command::Status(a, index));
}

unsafe extern "win64" fn q_enqueue_signal(q: Q, fence: *mut c_void, value: u64) {
    push(q, Command::Signal(fence as usize, value));
}

unsafe extern "win64" fn q_enqueue_set_event(q: Q, event: Handle) {
    push(q, Command::Event(event));
}

/// Runs one read: memory or file source into the destination address.
unsafe fn execute(r: &Request) -> Result<(), HRESULT> {
    if r.options & (OPT_COMPRESSION | OPT_TRANSFORMS | OPT_PAGE_DESTINATION | OPT_PHYSICAL_SOURCE)
        != 0
    {
        // TODO: zlib/BCPack, transforms, page and GPU destinations.
        return Err(E_NOTIMPL);
    }
    if r.destination.is_null() || r.source.is_null() {
        return Err(E_INVALIDARG);
    }
    let len = r.source_size as usize;
    if len > r.destination_size as usize {
        return Err(ERROR_INSUFFICIENT_BUFFER);
    }
    let dst = r.destination as *mut u8;
    if r.options & OPT_MEMORY_SOURCE != 0 {
        std::ptr::copy(r.source as *const u8, dst, len);
        return Ok(());
    }
    let fd = (*(r.source as *const FileObj))
        .data
        .fd
        .load(Ordering::Acquire);
    if fd < 0 {
        return Err(ERROR_INVALID_HANDLE);
    }
    let mut done = 0usize;
    while done < len {
        let n = libc::pread(
            fd,
            dst.add(done) as *mut c_void,
            len - done,
            (r.offset + done as u64) as libc::off_t,
        );
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return Err(from_errno(errno()));
        }
        if n == 0 {
            return Err(ERROR_HANDLE_EOF);
        }
        done += n as usize;
    }
    Ok(())
}

unsafe fn record_failure(q: &QueueState, f: Failure) {
    let first = {
        let mut g = lock(q);
        g.failures += 1;
        if g.first.is_none() {
            g.first = Some(f);
            true
        } else {
            false
        }
    };
    if first {
        let h = q.error_event.load(Ordering::Acquire);
        if h != 0 {
            set_event(h);
        }
    }
}

unsafe extern "win64" fn q_submit(q: Q) {
    let cmds = std::mem::take(&mut lock(&(*q).data).pending);
    let mut first_failure: Option<HRESULT> = None;
    for c in cmds {
        match c {
            Command::Read(r, name) => {
                if let Err(hr) = execute(&r) {
                    first_failure.get_or_insert(hr);
                    record_failure(
                        &(*q).data,
                        Failure {
                            hresult: hr,
                            command_type: 0,
                            name,
                            request: Some(r),
                        },
                    );
                }
            }
            Command::Status(a, i) => {
                let entries: &Vec<AtomicI32> = &(*a).data;
                if let Some(e) = entries.get(i as usize) {
                    e.store(first_failure.take().unwrap_or(S_OK), Ordering::Release);
                }
                release::<StatusVtbl, Vec<AtomicI32>>(a);
            }
            Command::Signal(fence, value) => {
                // TODO: signal the ID3D12Fence once fences exist.
                lock(&(*q).data).signals.push((fence, value));
            }
            Command::Event(h) => {
                set_event(h);
            }
        }
    }
}

/// Drops the commands whose tag matches `tag & mask == value`; status entries are left
/// to the title's own bookkeeping.
unsafe extern "win64" fn q_cancel(q: Q, mask: u64, value: u64) {
    let mut g = lock(&(*q).data);
    let mut kept = Vec::new();
    for c in std::mem::take(&mut g.pending) {
        match &c {
            Command::Read(r, _) if r.tag & mask == value => {}
            _ => kept.push(c),
        }
    }
    g.pending = kept;
}

unsafe extern "win64" fn q_close(q: Q) {
    let mut g = lock(&(*q).data);
    g.closed = true;
    for c in std::mem::take(&mut g.pending) {
        if let Command::Status(a, _) = c {
            release::<StatusVtbl, Vec<AtomicI32>>(a);
        }
    }
}

unsafe extern "win64" fn q_error_event(q: Q) -> Handle {
    let st = &(*q).data;
    let h = st.error_event.load(Ordering::Acquire);
    if h != 0 {
        return h;
    }
    let fresh = create_event(None, true, lock(st).first.is_some());
    match st
        .error_event
        .compare_exchange(0, fresh, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => fresh,
        Err(existing) => {
            close_handle(fresh);
            existing
        }
    }
}

/// Size of the error record written by `RetrieveErrorRecord`.
pub const ERROR_RECORD_SIZE: usize = 80 + size_of::<Request>();

unsafe extern "win64" fn q_error_record(q: Q, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let (failures, first) = {
        let mut g = lock(&(*q).data);
        (std::mem::take(&mut g.failures), g.first.take())
    };
    let mut rec = [0u8; ERROR_RECORD_SIZE];
    rec[0..4].copy_from_slice(&failures.to_le_bytes());
    if let Some(f) = first {
        rec[4..8].copy_from_slice(&f.hresult.to_le_bytes());
        rec[8..12].copy_from_slice(&f.command_type.to_le_bytes());
        let n = f.name.len().min(63);
        rec[12..12 + n].copy_from_slice(&f.name.as_bytes()[..n]);
        if let Some(r) = f.request {
            std::ptr::copy_nonoverlapping(
                &r as *const Request as *const u8,
                rec[80..].as_mut_ptr(),
                size_of::<Request>(),
            );
        }
    }
    std::ptr::copy_nonoverlapping(rec.as_ptr(), out, rec.len());
    let h = (*q).data.error_event.load(Ordering::Acquire);
    if h != 0 {
        reset_event(h);
    }
}

unsafe extern "win64" fn q_query(_q: Q, _info: *mut c_void) {
    // TODO: DSTORAGE_QUEUE_INFO (layout unconfirmed); nothing is written.
}

static QUEUE_VTBL: QueueVtbl = QueueVtbl {
    unknown: IUnknownVtbl::new(),
    enqueue_request: q_enqueue_request,
    enqueue_status: q_enqueue_status,
    enqueue_signal: q_enqueue_signal,
    submit: q_submit,
    cancel_requests_with_tag: q_cancel,
    close: q_close,
    get_error_event: q_error_event,
    retrieve_error_record: q_error_record,
    query: q_query,
    enqueue_set_event: q_enqueue_set_event,
};

/// The fences and values recorded by `EnqueueSignal` and run through `Submit` (diagnostics).
///
/// # Safety
/// `q` is a live queue object.
pub unsafe fn signals(q: *mut c_void) -> Vec<(usize, u64)> {
    lock(&(*(q as Q)).data).signals.clone()
}

// ------------------------------------------------------------------ factory

type FactoryObj = ComObject<FactoryVtbl, ()>;
type F = *mut FactoryObj;
type Out = *mut *mut c_void;

#[repr(C)]
pub struct FactoryVtbl {
    unknown: IUnknownVtbl<FactoryVtbl, ()>,
    unused3: unsafe extern "win64" fn(F) -> HRESULT,
    open_file: unsafe extern "win64" fn(F, *const u16, *const Guid, Out) -> HRESULT,
    create_status_array:
        unsafe extern "win64" fn(F, u32, *const c_char, *const Guid, Out) -> HRESULT,
    unused6: unsafe extern "win64" fn(F) -> HRESULT,
    set_debug_flags: unsafe extern "win64" fn(F, u32),
    set_staging_buffer_size: unsafe extern "win64" fn(F, u32) -> HRESULT,
    unused9: unsafe extern "win64" fn(F) -> HRESULT,
    create_queue: unsafe extern "win64" fn(F, *const QueueDesc, *const Guid, Out) -> HRESULT,
    unused11: unsafe extern "win64" fn(F) -> HRESULT,
    unused12: unsafe extern "win64" fn(F) -> HRESULT,
    unused13: unsafe extern "win64" fn(F) -> HRESULT,
    unused14: unsafe extern "win64" fn(F) -> HRESULT,
    unused15: unsafe extern "win64" fn(F) -> HRESULT,
}

unsafe extern "win64" fn f_unused(_: F) -> HRESULT {
    E_NOTIMPL
}

/// The IID list of a new object: the one asked for (so `QueryInterface` agrees with creation).
fn iids_for(iid: *const Guid) -> &'static [Guid] {
    if iid.is_null() {
        &[]
    } else {
        // SAFETY: non-null, a GUID from the title. A few bytes leak per object.
        Box::leak(Box::new([unsafe { *iid }]))
    }
}

unsafe fn put<V: 'static, T: 'static>(out: Out, obj: *mut ComObject<V, T>) -> HRESULT {
    *out = obj as *mut c_void;
    S_OK
}

unsafe extern "win64" fn f_open_file(
    _: F,
    path: *const u16,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(p) = nativ64::handles::name_w(path) else {
        return E_INVALIDARG;
    };
    let host = nativ64::paths::map_read_path(&p);
    let Ok(c) = std::ffi::CString::new(host.as_os_str().as_encoded_bytes()) else {
        return E_INVALIDARG;
    };
    let fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return from_errno(errno());
    }
    let obj = FileObj::create(
        &FILE_VTBL,
        iids_for(iid),
        FileState {
            fd: AtomicI32::new(fd),
        },
    );
    put(out, obj)
}

unsafe extern "win64" fn f_status_array(
    _: F,
    capacity: u32,
    _name: *const c_char,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let entries = (0..capacity).map(|_| AtomicI32::new(S_OK)).collect();
    put(out, StatusObj::create(&STATUS_VTBL, iids_for(iid), entries))
}

unsafe extern "win64" fn f_set_debug_flags(_: F, _flags: u32) {}

unsafe extern "win64" fn f_set_staging(_: F, _size: u32) -> HRESULT {
    S_OK
}

unsafe extern "win64" fn f_create_queue(
    _: F,
    desc: *const QueueDesc,
    iid: *const Guid,
    out: Out,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if desc.is_null() {
        return E_INVALIDARG;
    }
    put(
        out,
        QueueObj::create(&QUEUE_VTBL, iids_for(iid), QueueState::default()),
    )
}

static FACTORY_VTBL: FactoryVtbl = FactoryVtbl {
    unknown: IUnknownVtbl::new(),
    unused3: f_unused,
    open_file: f_open_file,
    create_status_array: f_status_array,
    unused6: f_unused,
    set_debug_flags: f_set_debug_flags,
    set_staging_buffer_size: f_set_staging,
    unused9: f_unused,
    create_queue: f_create_queue,
    unused11: f_unused,
    unused12: f_unused,
    unused13: f_unused,
    unused14: f_unused,
    unused15: f_unused,
};

static FACTORY: OnceLock<usize> = OnceLock::new();

/// The singleton factory; the process keeps its first reference.
fn factory() -> F {
    *FACTORY.get_or_init(|| FactoryObj::create(&FACTORY_VTBL, &FACTORY_IIDS, ()) as usize) as F
}

nativ64::win32! {
    module "xcrdapi";

    Native fn DStorageGetFactory(iid: *const Guid, out: Out) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        *out = std::ptr::null_mut();
        if iid.is_null() {
            return E_POINTER;
        }
        // The factory answers for its own IIDs only (not for IUnknown on this entry point).
        if !FACTORY_IIDS.contains(&*iid) {
            return E_NOINTERFACE;
        }
        nativ64::com::query_interface::<FactoryVtbl, ()>(factory(), iid, out as *mut F)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn vt<V>(o: *mut c_void) -> &'static V {
        &**(o as *const &V)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    struct Env {
        factory: F,
        queue: Q,
        status: *mut StatusObj,
    }

    unsafe fn env() -> Env {
        let mut f: *mut c_void = std::ptr::null_mut();
        assert_eq!(DStorageGetFactory(&IID_FACTORY_X, &mut f), S_OK);
        let factory = f as F;
        let desc = QueueDesc {
            source_type: 0,
            capacity: 64,
            priority: 0,
            name: std::ptr::null(),
            unknown: 1,
        };
        let mut q = std::ptr::null_mut();
        assert_eq!(
            (vt::<FactoryVtbl>(f).create_queue)(factory, &desc, std::ptr::null(), &mut q),
            S_OK
        );
        let mut s = std::ptr::null_mut();
        assert_eq!(
            (vt::<FactoryVtbl>(f).create_status_array)(
                factory,
                4,
                std::ptr::null(),
                std::ptr::null(),
                &mut s
            ),
            S_OK
        );
        Env {
            factory,
            queue: q as Q,
            status: s as *mut StatusObj,
        }
    }

    fn request(
        dst: &mut [u8],
        source: *const c_void,
        options: u64,
        offset: u64,
        size: u32,
    ) -> Request {
        Request {
            options,
            destination: dst.as_mut_ptr() as *mut c_void,
            page_offset: 0,
            destination_size: dst.len() as u32,
            source,
            offset,
            source_size: size,
            intermediate_size: 0,
            tag: 7,
            name: c"test".as_ptr(),
        }
    }

    #[test]
    fn factory_singleton_and_refs() {
        unsafe {
            let mut a: *mut c_void = std::ptr::null_mut();
            let mut b: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut a), S_OK);
            assert_eq!(DStorageGetFactory(&IID_FACTORY_X, &mut b), S_OK);
            assert_eq!(a, b);
            let bad = Guid::new(1, 2, 3, [4; 8]);
            assert_eq!(DStorageGetFactory(&bad, &mut b), E_NOINTERFACE);
            assert!(b.is_null());
            assert_eq!(
                DStorageGetFactory(&IID_FACTORY, std::ptr::null_mut()),
                E_POINTER
            );
            let before = (*(a as F)).ref_count();
            let v = vt::<FactoryVtbl>(a);
            assert_eq!((v.unknown.add_ref)(a as F), before + 1);
            assert_eq!((v.unknown.release)(a as F), before);
            assert_eq!((v.unused3)(a as F), E_NOTIMPL);
            assert_eq!((v.set_staging_buffer_size)(a as F, 32 << 20), S_OK);
            (v.set_debug_flags)(a as F, 1);
            (v.unknown.release)(a as F);
        }
    }

    #[test]
    fn memory_and_file_requests_complete() {
        unsafe {
            let e = env();
            let fv = vt::<FactoryVtbl>(e.factory as *mut c_void);
            let qv = vt::<QueueVtbl>(e.queue as *mut c_void);
            let sv = vt::<StatusVtbl>(e.status as *mut c_void);

            let path = std::env::temp_dir().join(format!("nativ64-ds-{}.bin", std::process::id()));
            let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
            std::fs::write(&path, &data).unwrap();
            let mut file = std::ptr::null_mut();
            let w = wide(path.to_str().unwrap());
            assert_eq!(
                (fv.open_file)(e.factory, w.as_ptr(), std::ptr::null(), &mut file),
                S_OK
            );
            let fobj = file as *mut FileObj;
            let mut info = [0u8; 52];
            assert_eq!(
                (vt::<FileVtbl>(file).get_file_information)(fobj, info.as_mut_ptr()),
                S_OK
            );
            assert_eq!(u32::from_le_bytes(info[40..44].try_into().unwrap()), 5000);

            let src = *b"hello memory";
            let mut m = [0u8; 16];
            let mut f = [0u8; 100];
            let r1 = request(
                &mut m,
                src.as_ptr() as *const c_void,
                OPT_MEMORY_SOURCE,
                0,
                12,
            );
            let r2 = request(&mut f, file as *const c_void, 0, 4000, 100);
            (qv.enqueue_request)(e.queue, &r1);
            (qv.enqueue_request)(e.queue, &r2);
            (qv.enqueue_status)(e.queue, e.status, 2);
            assert_eq!((sv.is_complete)(e.status, 2), 0);
            assert_eq!((sv.get_hresult)(e.status, 2), E_PENDING);
            assert_eq!((*e.status).ref_count(), 2);
            (qv.submit)(e.queue);
            assert_eq!((sv.is_complete)(e.status, 2), 1);
            assert_eq!((sv.get_hresult)(e.status, 2), S_OK);
            assert_eq!((sv.get_hresult)(e.status, 9), E_INVALIDARG);
            assert_eq!((*e.status).ref_count(), 1);
            assert_eq!(&m[..12], b"hello memory");
            assert_eq!(&f[..], &data[4000..4100]);

            // A read past the end of the file fails the next status entry.
            let mut big = [0u8; 200];
            let r3 = request(&mut big, file as *const c_void, 0, 4950, 200);
            (qv.enqueue_request)(e.queue, &r3);
            (qv.enqueue_status)(e.queue, e.status, 3);
            (qv.submit)(e.queue);
            assert_eq!((sv.get_hresult)(e.status, 3), ERROR_HANDLE_EOF);

            // Signals are recorded; events are set.
            let ev = create_event(None, true, false);
            (qv.enqueue_signal)(e.queue, 0x1000 as *mut c_void, 5);
            (qv.enqueue_set_event)(e.queue, ev);
            (qv.submit)(e.queue);
            assert_eq!(signals(e.queue as *mut c_void), vec![(0x1000, 5)]);
            assert_eq!(nativ64::handles::wait(&[ev], false, 0), 0);
            close_handle(ev);

            // Closed files fail; cancel drops by tag.
            (vt::<FileVtbl>(file).close)(fobj);
            (qv.enqueue_request)(e.queue, &r2);
            (qv.cancel_requests_with_tag)(e.queue, u64::MAX, 7);
            (qv.enqueue_status)(e.queue, e.status, 0);
            (qv.submit)(e.queue);
            assert_eq!((sv.get_hresult)(e.status, 0), S_OK);
            (qv.enqueue_request)(e.queue, &r2);
            (qv.enqueue_status)(e.queue, e.status, 1);
            (qv.submit)(e.queue);
            assert_eq!((sv.get_hresult)(e.status, 1), ERROR_INVALID_HANDLE);

            // Close discards what is queued, releasing the status reference.
            (qv.enqueue_status)(e.queue, e.status, 0);
            (qv.close)(e.queue);
            assert_eq!((*e.status).ref_count(), 1);

            assert_eq!((vt::<FileVtbl>(file).unknown.release)(fobj), 0);
            assert_eq!((qv.unknown.release)(e.queue), 0);
            assert_eq!((sv.unknown.release)(e.status), 0);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_file_and_unsupported_compression() {
        unsafe {
            let e = env();
            let fv = vt::<FactoryVtbl>(e.factory as *mut c_void);
            let qv = vt::<QueueVtbl>(e.queue as *mut c_void);
            let sv = vt::<StatusVtbl>(e.status as *mut c_void);
            let mut file: *mut c_void = std::ptr::dangling_mut::<c_void>();
            let w = wide("/nonexistent-nativ64/none.bin");
            assert_eq!(
                (fv.open_file)(e.factory, w.as_ptr(), std::ptr::null(), &mut file),
                ERROR_FILE_NOT_FOUND
            );
            assert!(file.is_null());

            let ev = (qv.get_error_event)(e.queue);
            assert_ne!(nativ64::handles::wait(&[ev], false, 0), 0);
            let src = [0u8; 8];
            let mut d = [0u8; 8];
            let r = request(
                &mut d,
                src.as_ptr() as *const c_void,
                OPT_MEMORY_SOURCE | 1,
                0,
                8,
            );
            (qv.enqueue_request)(e.queue, &r);
            (qv.enqueue_status)(e.queue, e.status, 0);
            (qv.submit)(e.queue);
            assert_eq!((sv.get_hresult)(e.status, 0), E_NOTIMPL);
            assert_eq!(nativ64::handles::wait(&[ev], false, 0), 0);
            let mut rec = [0u8; ERROR_RECORD_SIZE];
            (qv.retrieve_error_record)(e.queue, rec.as_mut_ptr());
            assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 1);
            assert_eq!(i32::from_le_bytes(rec[4..8].try_into().unwrap()), E_NOTIMPL);
            assert_eq!(&rec[12..16], b"test");
            assert_ne!(nativ64::handles::wait(&[ev], false, 0), 0);

            // Destination too small.
            let mut tiny = [0u8; 4];
            let r = request(
                &mut tiny,
                src.as_ptr() as *const c_void,
                OPT_MEMORY_SOURCE,
                0,
                8,
            );
            (qv.enqueue_request)(e.queue, &r);
            (qv.enqueue_status)(e.queue, e.status, 1);
            (qv.submit)(e.queue);
            assert_eq!((sv.get_hresult)(e.status, 1), ERROR_INSUFFICIENT_BUFFER);
        }
    }

    #[test]
    fn registered() {
        assert!(nativ64_lookup().is_some());
    }

    fn nativ64_lookup() -> Option<&'static nativ64::Export> {
        crate::lookup("XCRDAPI.dll", "DStorageGetFactory")
    }
}
