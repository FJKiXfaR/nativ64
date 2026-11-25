//! Process-wide handle table and kernel synchronization objects (not a Windows module).
//!
//! * Handles are non-zero multiples of 4 handed out by a monotonically increasing counter, so
//!   a closed value is never reused (until the 64-bit counter wraps) and `-1`/`-2` (the process
//!   and thread pseudo handles) can never be issued. Each handle maps to an `Arc<Object>`;
//!   `DuplicateHandle` adds another handle to the same object. `Object` is an enum so threads,
//!   files, ... can be added as variants without touching the table.
//! * One global "kernel lock" guards the table, the names and every object state transition.
//!   That keeps `WaitForMultipleObjects(bWaitAll)` atomic (all objects are checked and taken
//!   under the lock, or none). Per-object `std::sync::Mutex`es only provide interior
//!   mutability; they are always taken inside the kernel lock.
//! * Waiting: each waiter owns a futex word and registers in the waiter list of every object it
//!   waits on. A state change (`SetEvent`, release of a mutex or semaphore, owner thread exit)
//!   bumps the futex word of that object's waiters and wakes them; they re-check under the kernel
//!   lock. The word is read under the lock before checking and bumped under the lock, so a wakeup
//!   cannot be lost. No polling.
//! * Mutexes record their owner thread id; a thread-local guard marks the mutexes a thread still
//!   owns as abandoned when it exits.
//! * Names: a process-local namespace shared by all object kinds (`Weak` references, so a name
//!   is free once every handle is closed). Alertable waits are accepted and treated as
//!   non-alertable (no APCs yet). ANSI names are read as UTF-8 (lossy).

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::api_ms_win_core_synch::{futex_wait, futex_wake, timeout_of};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering::*};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Instant;

pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_NOT_OWNER: u32 = 288;
pub const ERROR_TOO_MANY_POSTS: u32 = 298;
pub const ERROR_ALREADY_EXISTS: u32 = 183;

pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_ABANDONED_0: u32 = 0x80;
pub const WAIT_TIMEOUT: u32 = 0x102;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
pub const MAXIMUM_WAIT_OBJECTS: u32 = 64;

pub type Handle = usize;

fn tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- objects

struct Waiter {
    futex: AtomicU32,
}

#[derive(Default)]
struct Waiters(Mutex<Vec<Arc<Waiter>>>);

impl Waiters {
    fn wake(&self) {
        for w in lock(&self.0).iter() {
            w.futex.fetch_add(1, SeqCst);
            futex_wake(&w.futex, i32::MAX);
        }
    }
}

struct EventState {
    manual: bool,
    signaled: bool,
}

struct MutexState {
    owner: u32, // thread id, 0 when free
    count: u32,
    abandoned: bool,
}

struct SemState {
    count: i32,
    max: i32,
}

struct Obj<S> {
    state: Mutex<S>,
    waiters: Waiters,
}

impl<S> Obj<S> {
    fn new(s: S) -> Self {
        Obj {
            state: Mutex::new(s),
            waiters: Waiters::default(),
        }
    }
}

enum Object {
    Event(Obj<EventState>),
    Mutex(Obj<MutexState>),
    Semaphore(Obj<SemState>),
}

#[derive(PartialEq)]
enum Taken {
    Normal,
    Abandoned,
}

impl Object {
    fn waiters(&self) -> &Waiters {
        match self {
            Object::Event(o) => &o.waiters,
            Object::Mutex(o) => &o.waiters,
            Object::Semaphore(o) => &o.waiters,
        }
    }

    fn same_kind(&self, other: &Object) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// Would a wait by thread `me` succeed right now? (Kernel lock held.)
    fn can_acquire(&self, me: u32) -> bool {
        match self {
            Object::Event(o) => lock(&o.state).signaled,
            Object::Mutex(o) => {
                let s = lock(&o.state);
                s.owner == 0 || s.owner == me
            }
            Object::Semaphore(o) => lock(&o.state).count > 0,
        }
    }

    /// Takes the object for `me`; only valid after `can_acquire`. (Kernel lock held.)
    fn acquire(self: &Arc<Self>, me: u32) -> Taken {
        match &**self {
            Object::Event(o) => {
                let mut s = lock(&o.state);
                if !s.manual {
                    s.signaled = false;
                }
                Taken::Normal
            }
            Object::Mutex(o) => {
                let mut s = lock(&o.state);
                let first = s.owner == 0;
                s.owner = me;
                s.count += 1;
                if first {
                    OWNED.with(|l| l.0.borrow_mut().push(Arc::downgrade(self)));
                }
                if std::mem::take(&mut s.abandoned) {
                    Taken::Abandoned
                } else {
                    Taken::Normal
                }
            }
            Object::Semaphore(o) => {
                let mut s = lock(&o.state);
                s.count = (s.count - 1).max(0);
                Taken::Normal
            }
        }
    }
}

/// Mutexes owned by the current thread; abandoned when the thread exits.
struct OwnedMutexes(RefCell<Vec<Weak<Object>>>);

thread_local! {
    static OWNED: OwnedMutexes = const { OwnedMutexes(RefCell::new(Vec::new())) };
}

impl Drop for OwnedMutexes {
    fn drop(&mut self) {
        let list = std::mem::take(&mut *self.0.borrow_mut());
        if list.is_empty() {
            return;
        }
        let me = tid();
        let _k = kernel();
        for o in list.iter().filter_map(Weak::upgrade) {
            if let Object::Mutex(m) = &*o {
                let mut s = lock(&m.state);
                if s.owner == me {
                    *s = MutexState {
                        owner: 0,
                        count: 0,
                        abandoned: true,
                    };
                    drop(s);
                    m.waiters.wake();
                }
            }
        }
    }
}

// ---------------------------------------------------------------- table

struct Kernel {
    table: HashMap<Handle, Arc<Object>>,
    names: HashMap<String, Weak<Object>>,
    next: Handle,
}

static KERNEL: LazyLock<Mutex<Kernel>> = LazyLock::new(|| {
    Mutex::new(Kernel {
        table: HashMap::new(),
        names: HashMap::new(),
        next: 4,
    })
});

fn kernel() -> MutexGuard<'static, Kernel> {
    lock(&KERNEL)
}

impl Kernel {
    fn insert(&mut self, o: Arc<Object>) -> Handle {
        let h = self.next;
        self.next = self.next.wrapping_add(4).max(4);
        self.table.insert(h, o);
        h
    }

    fn get(&self, h: Handle) -> Option<Arc<Object>> {
        self.table.get(&h).cloned()
    }
}

/// Creates (or opens, by name) an object and returns a new handle. Sets the last error to
/// `ERROR_ALREADY_EXISTS` when an existing named object was opened, 0 on a fresh creation;
/// a name used by an object of another kind fails with `ERROR_INVALID_HANDLE`.
fn create(name: Option<String>, make: impl FnOnce() -> Object) -> (Handle, bool) {
    let new = make();
    let mut k = kernel();
    if let Some(n) = &name {
        if let Some(existing) = k.names.get(n).and_then(Weak::upgrade) {
            if !existing.same_kind(&new) {
                set_last_error(ERROR_INVALID_HANDLE);
                return (0, false);
            }
            let h = k.insert(existing);
            set_last_error(ERROR_ALREADY_EXISTS);
            return (h, false);
        }
    }
    let o = Arc::new(new);
    if let Some(n) = name {
        k.names.retain(|_, w| w.strong_count() > 0);
        k.names.insert(n, Arc::downgrade(&o));
    }
    let h = k.insert(o);
    set_last_error(0);
    (h, true)
}

// ---------------------------------------------------------------- names

/// # Safety
/// `p` must be NULL or a NUL-terminated UTF-16 string.
pub unsafe fn name_w(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)))
}

/// # Safety
/// `p` must be NULL or a NUL-terminated string.
pub unsafe fn name_a(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(
        std::ffi::CStr::from_ptr(p as *const _)
            .to_string_lossy()
            .into_owned(),
    )
}

// ---------------------------------------------------------------- events

pub fn create_event(name: Option<String>, manual: bool, initial: bool) -> Handle {
    create(name, || {
        Object::Event(Obj::new(EventState {
            manual,
            signaled: initial,
        }))
    })
    .0
}

fn event_state(h: Handle, signaled: bool) -> i32 {
    let k = kernel();
    let obj = k.get(h);
    match obj.as_deref() {
        Some(Object::Event(e)) => {
            lock(&e.state).signaled = signaled;
            if signaled {
                e.waiters.wake();
            }
            1
        }
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            0
        }
    }
}

pub fn set_event(h: Handle) -> i32 {
    event_state(h, true)
}

pub fn reset_event(h: Handle) -> i32 {
    event_state(h, false)
}

// ---------------------------------------------------------------- mutexes

pub fn create_mutex(name: Option<String>, initial_owner: bool) -> Handle {
    let me = tid();
    let (h, fresh) = create(name, || {
        Object::Mutex(Obj::new(MutexState {
            owner: if initial_owner { me } else { 0 },
            count: initial_owner as u32,
            abandoned: false,
        }))
    });
    // Only a freshly created mutex is owned at creation; remember it for abandonment.
    if fresh && initial_owner {
        if let Some(o) = kernel().get(h) {
            OWNED.with(|l| l.0.borrow_mut().push(Arc::downgrade(&o)));
        }
    }
    h
}

pub fn release_mutex(h: Handle) -> i32 {
    let k = kernel();
    let obj = k.get(h);
    let Some(Object::Mutex(m)) = obj.as_deref() else {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let me = tid();
    let mut s = lock(&m.state);
    if s.owner != me {
        set_last_error(ERROR_NOT_OWNER);
        return 0;
    }
    s.count -= 1;
    if s.count == 0 {
        s.owner = 0;
        drop(s);
        let me_ptr = m as *const Obj<MutexState>;
        OWNED.with(|l| {
            l.0.borrow_mut().retain(|w| match w.upgrade() {
                Some(o) => !matches!(&*o, Object::Mutex(x) if std::ptr::eq(x, me_ptr)),
                None => false,
            })
        });
        m.waiters.wake();
    }
    1
}

// ---------------------------------------------------------------- semaphores

pub fn create_semaphore(name: Option<String>, initial: i32, max: i32) -> Handle {
    if max <= 0 || initial < 0 || initial > max {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    create(name, || {
        Object::Semaphore(Obj::new(SemState {
            count: initial,
            max,
        }))
    })
    .0
}

/// Returns `(ok, previous count)`; the previous count is only reported on success.
pub fn release_semaphore(h: Handle, n: i32) -> (i32, Option<i32>) {
    let k = kernel();
    let obj = k.get(h);
    let Some(Object::Semaphore(s)) = obj.as_deref() else {
        set_last_error(ERROR_INVALID_HANDLE);
        return (0, None);
    };
    if n <= 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return (0, None);
    }
    let mut st = lock(&s.state);
    let prev = st.count;
    if prev as i64 + n as i64 > st.max as i64 {
        set_last_error(ERROR_TOO_MANY_POSTS);
        return (0, None);
    }
    st.count += n;
    drop(st);
    s.waiters.wake();
    (1, Some(prev))
}

// ---------------------------------------------------------------- handles

pub fn close_handle(h: Handle) -> i32 {
    let removed = kernel().table.remove(&h);
    if removed.is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    1
}

pub const DUPLICATE_CLOSE_SOURCE: u32 = 1;

/// Same-process duplication: a new handle to the same object. Rights are not modelled.
pub fn duplicate_handle(src: Handle, close_source: bool) -> Option<Handle> {
    let mut k = kernel();
    let Some(o) = k.get(src) else {
        set_last_error(ERROR_INVALID_HANDLE);
        return None;
    };
    let h = k.insert(o);
    if close_source {
        k.table.remove(&src);
    }
    Some(h)
}

// ---------------------------------------------------------------- waiting

pub fn wait(handles: &[Handle], wait_all: bool, ms: u32) -> u32 {
    let me = tid();
    let deadline = (ms != u32::MAX).then(|| Instant::now() + timeout_of(ms).unwrap());
    let waiter = Arc::new(Waiter {
        futex: AtomicU32::new(0),
    });

    let mut k = kernel();
    let mut objs = Vec::with_capacity(handles.len());
    for &h in handles {
        match k.get(h) {
            Some(o) => objs.push(o),
            None => {
                set_last_error(ERROR_INVALID_HANDLE);
                return WAIT_FAILED;
            }
        }
    }

    let mut registered = false;
    let result = loop {
        let seq = waiter.futex.load(SeqCst);
        let done = if wait_all {
            objs.iter()
                .all(|o| o.can_acquire(me))
                .then(|| take_all(&objs, me))
        } else {
            objs.iter()
                .position(|o| o.can_acquire(me))
                .map(|i| take_one(&objs[i], me, i))
        };
        if let Some(r) = done {
            break r;
        }
        let remaining = match deadline {
            None => None,
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    break WAIT_TIMEOUT;
                }
                Some(d - now)
            }
        };
        if !registered {
            for o in &objs {
                lock(&o.waiters().0).push(waiter.clone());
            }
            registered = true;
        }
        drop(k);
        futex_wait(&waiter.futex, seq, remaining);
        k = kernel();
    };
    if registered {
        for o in &objs {
            lock(&o.waiters().0).retain(|w| !Arc::ptr_eq(w, &waiter));
        }
    }
    drop(k);
    result
}

fn take_one(o: &Arc<Object>, me: u32, i: usize) -> u32 {
    match o.acquire(me) {
        Taken::Normal => WAIT_OBJECT_0 + i as u32,
        Taken::Abandoned => WAIT_ABANDONED_0 + i as u32,
    }
}

fn take_all(objs: &[Arc<Object>], me: u32) -> u32 {
    let mut result = WAIT_OBJECT_0;
    for (i, o) in objs.iter().enumerate() {
        if o.acquire(me) == Taken::Abandoned && result == WAIT_OBJECT_0 {
            result = WAIT_ABANDONED_0 + i as u32;
        }
    }
    result
}

/// # Safety
/// `handles` must be NULL or point to `count` handles.
pub unsafe fn wait_multiple(count: u32, handles: *const Handle, wait_all: bool, ms: u32) -> u32 {
    if count == 0 || count > MAXIMUM_WAIT_OBJECTS || handles.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    wait(
        std::slice::from_raw_parts(handles, count as usize),
        wait_all,
        ms,
    )
}

#[cfg(test)]
mod tests {
    use super::{WAIT_ABANDONED_0, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use crate::api_ms_win_core_errorhandling::{GetLastError, SetLastError};
    use crate::api_ms_win_core_handle::*;
    use crate::api_ms_win_core_synch::*;
    use crate::lookup;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const INF: u32 = 0xFFFF_FFFF;
    const CUR: usize = usize::MAX;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }
    fn event(manual: bool, set: bool) -> usize {
        unsafe { CreateEventW(null(), manual as i32, set as i32, null()) }
    }
    fn mutex(owned: bool) -> usize {
        unsafe { CreateMutexW(null(), owned as i32, null()) }
    }
    fn sem(initial: i32, max: i32) -> usize {
        unsafe { CreateSemaphoreW(null(), initial, max, null()) }
    }
    fn poll(h: usize) -> u32 {
        unsafe { WaitForSingleObject(h, 0) }
    }
    fn sleep() {
        std::thread::sleep(Duration::from_millis(60));
    }
    /// Runs `f` on a thread that has not finished before the caller continues.
    fn spawn<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::spawn(f)
    }

    #[test]
    fn handle_values_and_closing() {
        let h = event(false, false);
        assert!(h != 0 && h.is_multiple_of(4) && h != CUR && h != CUR - 1);
        unsafe {
            assert_eq!(CloseHandle(h), 1);
            SetLastError(0);
            assert_eq!(WaitForSingleObject(h, 0), WAIT_FAILED);
            assert_eq!(GetLastError(), 6);
            SetLastError(0);
            assert_eq!(CloseHandle(h), 0);
            assert_eq!(GetLastError(), 6);
            assert_eq!(CloseHandle(0), 0);
            assert_eq!(CloseHandle(CUR), 0);
            assert_eq!(CloseHandle(CUR - 1), 0);
            assert_eq!(WaitForSingleObject(CUR - 1, 0), WAIT_FAILED);
            assert_eq!(SetEvent(h), 0);
            assert_eq!(GetLastError(), 6);
        }
        let h2 = event(false, false);
        assert_ne!(h, h2);
        unsafe { CloseHandle(h2) };
    }

    #[test]
    fn wrong_kind_is_invalid_handle() {
        let e = event(false, false);
        let m = mutex(false);
        unsafe {
            assert_eq!(ReleaseMutex(e), 0);
            assert_eq!(GetLastError(), 6);
            assert_eq!(SetEvent(m), 0);
            assert_eq!(GetLastError(), 6);
            assert_eq!(ReleaseSemaphore(e, 1, null_mut()), 0);
            assert_eq!(GetLastError(), 6);
        }
    }

    #[test]
    fn auto_reset_event_wakes_one_waiter_per_set() {
        let e = event(false, false);
        let woke = Arc::new(AtomicU32::new(0));
        let ts: Vec<_> = (0..3)
            .map(|_| {
                let woke = woke.clone();
                spawn(move || {
                    assert_eq!(unsafe { WaitForSingleObject(e, INF) }, 0);
                    woke.fetch_add(1, SeqCst);
                })
            })
            .collect();
        sleep();
        assert_eq!(woke.load(SeqCst), 0);
        for want in 1..=3 {
            unsafe { SetEvent(e) };
            sleep();
            assert_eq!(woke.load(SeqCst), want);
            assert_eq!(poll(e), WAIT_TIMEOUT, "auto-reset consumed by the waiter");
        }
        ts.into_iter().for_each(|t| t.join().unwrap());
    }

    #[test]
    fn auto_reset_initial_state_is_consumed_once() {
        let e = event(false, true);
        assert_eq!(poll(e), 0);
        assert_eq!(poll(e), WAIT_TIMEOUT);
    }

    #[test]
    fn manual_reset_event_wakes_all_and_stays_set() {
        let e = event(true, false);
        let woke = Arc::new(AtomicU32::new(0));
        let ts: Vec<_> = (0..4)
            .map(|_| {
                let woke = woke.clone();
                spawn(move || {
                    assert_eq!(unsafe { WaitForSingleObject(e, INF) }, 0);
                    woke.fetch_add(1, SeqCst);
                })
            })
            .collect();
        sleep();
        assert_eq!(woke.load(SeqCst), 0);
        unsafe { SetEvent(e) };
        ts.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(woke.load(SeqCst), 4);
        assert_eq!(poll(e), 0);
        assert_eq!(poll(e), 0);
        unsafe { ResetEvent(e) };
        assert_eq!(poll(e), WAIT_TIMEOUT);
        let ex = unsafe { CreateEventExW(null(), null(), 3, 0) };
        assert_eq!(poll(ex), 0);
        assert_eq!(poll(ex), 0, "flags 3 = manual reset + initially set");
    }

    #[test]
    fn timeouts() {
        let e = event(false, false);
        let t = Instant::now();
        assert_eq!(unsafe { WaitForSingleObject(e, 80) }, WAIT_TIMEOUT);
        let d = t.elapsed();
        assert!(
            d >= Duration::from_millis(80) && d < Duration::from_millis(1500),
            "{d:?}"
        );
        let t = Instant::now();
        assert_eq!(poll(e), WAIT_TIMEOUT);
        assert!(t.elapsed() < Duration::from_millis(50));
        // Spurious wakes (a set/consume race) must not shorten the wait.
        let e2 = event(false, false);
        let t = Instant::now();
        let h = spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(20));
                unsafe { SetEvent(e2) };
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let w = unsafe { WaitForSingleObjectEx(e2, 100, 1) };
        assert_eq!(w, 0);
        h.join().unwrap();
        assert!(t.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn mutex_recursion_and_ownership() {
        let m = mutex(false);
        assert_eq!(poll(m), 0);
        assert_eq!(poll(m), 0);
        let other = spawn(move || {
            assert_eq!(poll(m), WAIT_TIMEOUT, "owned by the main thread");
            assert_eq!(unsafe { ReleaseMutex(m) }, 0);
            assert_eq!(unsafe { GetLastError() }, 288);
        });
        other.join().unwrap();
        unsafe {
            assert_eq!(ReleaseMutex(m), 1);
            // Still owned (recursion count 1): not available to others yet.
            assert_eq!(spawn(move || poll(m)).join().unwrap(), WAIT_TIMEOUT);
            assert_eq!(ReleaseMutex(m), 1);
            assert_eq!(ReleaseMutex(m), 0);
            assert_eq!(GetLastError(), 288);
        }
        assert_eq!(spawn(move || poll(m)).join().unwrap(), 0);
    }

    #[test]
    fn initially_owned_mutex_blocks_then_hands_over() {
        let m = mutex(true);
        let t = spawn(move || unsafe {
            let r = WaitForSingleObject(m, INF);
            ReleaseMutex(m);
            r
        });
        sleep();
        assert!(!t.is_finished());
        unsafe { ReleaseMutex(m) };
        assert_eq!(t.join().unwrap(), 0);
    }

    #[test]
    fn mutex_abandoned_when_owner_thread_exits() {
        let m = mutex(false);
        let (owned_tx, owned_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let owner = spawn(move || {
            assert_eq!(poll(m), 0);
            assert_eq!(poll(m), 0);
            owned_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // Exits without releasing.
        });
        owned_rx.recv().unwrap();
        let waiter = spawn(move || unsafe { WaitForSingleObject(m, INF) });
        sleep();
        assert!(!waiter.is_finished());
        go_tx.send(()).unwrap();
        owner.join().unwrap();
        let r = waiter.join().unwrap();
        assert_eq!(r, WAIT_ABANDONED_0);
        // The waiter thread also exited owning it: abandoned again for the next one.
        assert_eq!(poll(m), WAIT_ABANDONED_0);
        assert_eq!(poll(m), 0, "the abandoned flag is consumed");
        assert_eq!(unsafe { ReleaseMutex(m) }, 1);
    }

    #[test]
    fn released_mutex_is_not_abandoned() {
        let m = mutex(false);
        spawn(move || unsafe {
            WaitForSingleObject(m, 0);
            ReleaseMutex(m);
        })
        .join()
        .unwrap();
        assert_eq!(poll(m), 0);
    }

    #[test]
    fn semaphore_counts_and_overflow() {
        let s = sem(1, 3);
        unsafe {
            assert_eq!(poll(s), 0);
            assert_eq!(poll(s), WAIT_TIMEOUT);
            let mut prev = -1;
            assert_eq!(ReleaseSemaphore(s, 2, &mut prev), 1);
            assert_eq!(prev, 0);
            assert_eq!(ReleaseSemaphore(s, 1, &mut prev), 1);
            assert_eq!(prev, 2);
            SetLastError(0);
            assert_eq!(ReleaseSemaphore(s, 1, &mut prev), 0);
            assert_eq!(GetLastError(), 298);
            // Behaviour per Wine's dlls/kernel32/tests/sync.c (run on Windows): a failed
            // release leaves the caller's previous-count slot untouched.
            assert_eq!(prev, 2);
            assert_eq!(ReleaseSemaphore(s, 0, &mut prev), 0);
            assert_eq!(GetLastError(), 87);
            assert_eq!(prev, 2);
            assert_eq!(ReleaseSemaphore(s, -1, null_mut()), 0);
            assert_eq!(poll(s), 0);
            assert_eq!(poll(s), 0);
            assert_eq!(poll(s), 0);
            assert_eq!(poll(s), WAIT_TIMEOUT);
            assert_eq!(sem(2, 1), 0);
            assert_eq!(GetLastError(), 87);
            assert_eq!(sem(-1, 1), 0);
            assert_eq!(sem(0, 0), 0);
            let ex = CreateSemaphoreExW(null(), 1, 1, null(), 0, 0);
            assert_ne!(ex, 0);
            assert_eq!(poll(ex), 0);
        }
    }

    #[test]
    fn semaphore_release_wakes_waiters() {
        let s = sem(0, 10);
        let woke = Arc::new(AtomicU32::new(0));
        let ts: Vec<_> = (0..3)
            .map(|_| {
                let woke = woke.clone();
                spawn(move || {
                    assert_eq!(unsafe { WaitForSingleObject(s, INF) }, 0);
                    woke.fetch_add(1, SeqCst);
                })
            })
            .collect();
        sleep();
        unsafe { ReleaseSemaphore(s, 2, null_mut()) };
        sleep();
        assert_eq!(woke.load(SeqCst), 2);
        unsafe { ReleaseSemaphore(s, 1, null_mut()) };
        ts.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(woke.load(SeqCst), 3);
    }

    #[test]
    fn wait_any_returns_the_index() {
        let hs = [event(true, false), event(true, false), event(true, false)];
        unsafe {
            assert_eq!(WaitForMultipleObjects(3, hs.as_ptr(), 0, 0), WAIT_TIMEOUT);
            SetEvent(hs[2]);
            assert_eq!(
                WaitForMultipleObjects(3, hs.as_ptr(), 0, 0),
                WAIT_OBJECT_0 + 2
            );
            SetEvent(hs[1]);
            assert_eq!(
                WaitForMultipleObjects(3, hs.as_ptr(), 0, 0),
                WAIT_OBJECT_0 + 1,
                "lowest index wins"
            );
        }
        let hs = [event(true, false), event(true, false)];
        let t = spawn(move || {
            sleep();
            unsafe { SetEvent(hs[1]) };
        });
        assert_eq!(
            unsafe { WaitForMultipleObjectsEx(2, hs.as_ptr(), 0, INF, 0) },
            WAIT_OBJECT_0 + 1
        );
        t.join().unwrap();
    }

    #[test]
    fn wait_any_abandoned_index() {
        let e = event(true, false);
        let m = mutex(false);
        spawn(move || {
            poll(m);
        })
        .join()
        .unwrap();
        let hs = [e, m];
        assert_eq!(
            unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 0, 0) },
            WAIT_ABANDONED_0 + 1
        );
    }

    #[test]
    fn wait_all_is_atomic() {
        let a = event(false, true); // auto-reset, signaled
        let s = sem(1, 1);
        let b = event(true, false);
        let hs = [a, s, b];
        unsafe {
            assert_eq!(WaitForMultipleObjects(3, hs.as_ptr(), 1, 50), WAIT_TIMEOUT);
            // Nothing was taken by the failed wait.
            assert_eq!(poll(a), 0);
            assert_eq!(poll(s), 0);
            ReleaseSemaphore(s, 1, null_mut());
            SetEvent(a);
            SetEvent(b);
            assert_eq!(WaitForMultipleObjects(3, hs.as_ptr(), 1, 0), WAIT_OBJECT_0);
            // All were taken together.
            assert_eq!(poll(a), WAIT_TIMEOUT);
            assert_eq!(poll(s), WAIT_TIMEOUT);
            assert_eq!(poll(b), 0, "manual reset stays set");
        }
    }

    #[test]
    fn wait_all_sleeps_until_every_object_is_ready() {
        let a = event(false, false);
        let b = event(false, false);
        let hs = [a, b];
        let t = spawn(move || unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 1, INF) });
        sleep();
        unsafe { SetEvent(a) };
        sleep();
        assert!(!t.is_finished());
        assert_eq!(poll(a), 0, "a is still ours: the waiter holds nothing");
        unsafe { SetEvent(a) };
        unsafe { SetEvent(b) };
        assert_eq!(t.join().unwrap(), WAIT_OBJECT_0);
        assert_eq!(poll(a), WAIT_TIMEOUT);
        assert_eq!(poll(b), WAIT_TIMEOUT);
    }

    #[test]
    fn wait_all_with_abandoned_mutex() {
        let m = mutex(false);
        spawn(move || {
            poll(m);
        })
        .join()
        .unwrap();
        let e = event(false, true);
        let hs = [e, m];
        assert_eq!(
            unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 1, 0) },
            WAIT_ABANDONED_0 + 1
        );
        assert_eq!(poll(e), WAIT_TIMEOUT);
    }

    #[test]
    fn wait_argument_errors() {
        let e = event(false, true);
        let hs = [e; 65];
        unsafe {
            SetLastError(0);
            assert_eq!(WaitForMultipleObjects(0, hs.as_ptr(), 0, 0), WAIT_FAILED);
            assert_eq!(GetLastError(), 87);
            assert_eq!(WaitForMultipleObjects(65, hs.as_ptr(), 0, 0), WAIT_FAILED);
            assert_eq!(WaitForMultipleObjects(1, null(), 0, 0), WAIT_FAILED);
            assert_eq!(WaitForMultipleObjects(64, hs.as_ptr(), 0, 0), 0);
            let bad = [e, 0x1234];
            SetLastError(0);
            assert_eq!(WaitForMultipleObjects(2, bad.as_ptr(), 0, 0), WAIT_FAILED);
            assert_eq!(GetLastError(), 6);
        }
    }

    #[test]
    fn named_objects() {
        let n = wide("nativ64-test-named-event");
        unsafe {
            let a = CreateEventW(null(), 1, 0, n.as_ptr());
            assert_ne!(a, 0);
            assert_eq!(GetLastError(), 0);
            let b = CreateEventW(null(), 0, 1, n.as_ptr());
            assert_ne!(b, 0);
            assert_ne!(a, b);
            assert_eq!(GetLastError(), 183);
            // Same object: the existing one is opened, its type (manual, not set) is kept.
            assert_eq!(poll(b), WAIT_TIMEOUT);
            SetEvent(a);
            assert_eq!(poll(b), 0);
            assert_eq!(poll(b), 0);
            // Another kind under the same name fails.
            assert_eq!(CreateMutexW(null(), 0, n.as_ptr()), 0);
            assert_eq!(GetLastError(), 6);
            assert_eq!(CreateSemaphoreW(null(), 0, 1, n.as_ptr()), 0);
            // ANSI variant, same namespace.
            let c = CreateEventA(
                null(),
                0,
                0,
                c"nativ64-test-named-event".as_ptr() as *const u8,
            );
            assert_ne!(c, 0);
            assert_eq!(GetLastError(), 183);
            // The name lives while any handle does, then is free.
            CloseHandle(a);
            CloseHandle(b);
            assert_eq!(poll(c), 0);
            CloseHandle(c);
            let d = CreateEventW(null(), 1, 0, n.as_ptr());
            assert_eq!(GetLastError(), 0);
            assert_eq!(poll(d), WAIT_TIMEOUT, "a fresh object, not the old one");
            // An initial owner is ignored when opening an existing mutex.
            let mn = wide("nativ64-test-named-mutex");
            let m1 = CreateMutexW(null(), 1, mn.as_ptr());
            let m2 = CreateMutexW(null(), 0, mn.as_ptr());
            assert_eq!(GetLastError(), 183);
            assert_eq!(spawn(move || poll(m2)).join().unwrap(), WAIT_TIMEOUT);
            ReleaseMutex(m1);
            assert_eq!(spawn(move || poll(m2)).join().unwrap(), 0);
        }
    }

    #[test]
    fn duplicate_handle_keeps_the_object_alive() {
        let e = event(true, false);
        let mut d = 0usize;
        unsafe {
            assert_eq!(DuplicateHandle(CUR, e, CUR, &mut d, 0, 0, 2), 1);
            assert!(d != 0 && d != e && d.is_multiple_of(4));
            assert_eq!(CloseHandle(e), 1);
            assert_eq!(poll(e), WAIT_FAILED);
            assert_eq!(SetEvent(d), 1);
            assert_eq!(poll(d), 0);
            // CLOSE_SOURCE
            let mut d2 = 0usize;
            assert_eq!(DuplicateHandle(CUR, d, CUR, &mut d2, 0, 0, 1 | 2), 1);
            assert_eq!(poll(d), WAIT_FAILED);
            assert_eq!(poll(d2), 0);
            assert_eq!(DuplicateHandle(CUR, d2, CUR, null_mut(), 0, 0, 1), 1);
            assert_eq!(poll(d2), WAIT_FAILED);
            // Errors
            SetLastError(0);
            assert_eq!(DuplicateHandle(CUR, d2, CUR, &mut d, 0, 0, 2), 0);
            assert_eq!(GetLastError(), 6);
            let live = event(false, false);
            assert_eq!(DuplicateHandle(CUR, live, CUR, null_mut(), 0, 0, 0), 0);
            assert_eq!(GetLastError(), 87);
            assert_eq!(DuplicateHandle(0x1000, live, CUR, &mut d, 0, 0, 0), 0);
            assert_eq!(DuplicateHandle(CUR, CUR, CUR, &mut d, 0, 0, 0), 0);
        }
    }

    #[test]
    fn exported_through_lookup_with_win64_pointers() {
        for m in ["api-ms-win-core-synch-l1-2-0.dll", "KERNEL32.dll"] {
            for f in [
                "CreateEventW",
                "CreateEventA",
                "CreateEventExW",
                "SetEvent",
                "ResetEvent",
                "CreateMutexW",
                "CreateMutexA",
                "ReleaseMutex",
                "CreateSemaphoreW",
                "CreateSemaphoreExW",
                "ReleaseSemaphore",
                "WaitForSingleObject",
                "WaitForSingleObjectEx",
                "WaitForMultipleObjects",
                "WaitForMultipleObjectsEx",
                "CloseHandle",
                "DuplicateHandle",
            ] {
                let e = lookup(m, f);
                if f == "CloseHandle" || f == "DuplicateHandle" {
                    assert!(lookup("api-ms-win-core-handle-l1-1-0", f).is_some(), "{f}");
                    assert!(lookup("kernel32", f).is_some(), "{f}");
                    continue;
                }
                assert!(e.is_some(), "{m}!{f}");
            }
        }
        type Create = unsafe extern "win64" fn(*const (), i32, i32, *const u16) -> usize;
        type Set = unsafe extern "win64" fn(usize) -> i32;
        type Wait = unsafe extern "win64" fn(usize, u32) -> u32;
        type Close = unsafe extern "win64" fn(usize) -> i32;
        unsafe {
            let create: Create =
                std::mem::transmute(lookup("kernel32", "CreateEventW").unwrap().addr);
            let set: Set = std::mem::transmute(lookup("kernel32", "SetEvent").unwrap().addr);
            let wait: Wait =
                std::mem::transmute(lookup("kernel32", "WaitForSingleObject").unwrap().addr);
            let close: Close = std::mem::transmute(lookup("kernel32", "CloseHandle").unwrap().addr);
            let h = create(null(), 0, 0, null());
            assert_eq!(wait(h, 0), WAIT_TIMEOUT);
            assert_eq!(set(h), 1);
            assert_eq!(wait(h, 100), 0);
            assert_eq!(close(h), 1);
            assert_eq!(wait(h, 0), WAIT_FAILED);
        }
    }
}
