//! `ws2_32`: Winsock start-up, the per-thread error code and the socket calls the title uses.
//!
//! Sockets are the host's: a Win32 `SOCKET` is a Linux file descriptor, `sockaddr_in`,
//! `SOCK_STREAM` and `IPPROTO_TCP` have the same values, and the calls that differ
//! (`select`'s `fd_set`, `ioctlsocket`'s commands, the `SOL_SOCKET` option numbers, `errno`
//! to `WSAE*`) are translated here.
//!
//! `WSAStartup` negotiates down to Winsock 2.2 (requested major 0 or above 2 is
//! `WSAVERNOTSUPPORTED`), fills `WSADATA` (64-bit layout, 408 bytes) and counts the call;
//! `WSACleanup` undoes one (`WSANOTINITIALISED` and `SOCKET_ERROR` when unbalanced).
//! Imports by ordinal resolve through [`crate::lookup_ordinal`]. No Wine oracle is vendored.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

pub const WSAVERNOTSUPPORTED: i32 = 10092;
pub const WSANOTINITIALISED: i32 = 10093;
pub const WSAEINVAL: i32 = 10022;
pub const WSAEFAULT: i32 = 10014;
pub const WSAHOST_NOT_FOUND: i32 = 11001;
pub const WSAEINTR: i32 = 10004;
pub const WSAEBADF: i32 = 10009;
pub const WSAEACCES: i32 = 10013;
pub const WSAENOTSOCK: i32 = 10038;
pub const WSAEDESTADDRREQ: i32 = 10039;
pub const WSAEWOULDBLOCK: i32 = 10035;
pub const WSAEINPROGRESS: i32 = 10036;
pub const WSAEALREADY: i32 = 10037;
pub const WSAEMSGSIZE: i32 = 10040;
pub const WSAEPROTOTYPE: i32 = 10041;
pub const WSAEAFNOSUPPORT: i32 = 10047;
pub const WSAEPROTONOSUPPORT: i32 = 10043;
pub const WSAEOPNOTSUPP: i32 = 10045;
pub const WSAEADDRINUSE: i32 = 10048;
pub const WSAEADDRNOTAVAIL: i32 = 10049;
pub const WSAENETDOWN: i32 = 10050;
pub const WSAENETUNREACH: i32 = 10051;
pub const WSAENETRESET: i32 = 10052;
pub const WSAECONNABORTED: i32 = 10053;
pub const WSAECONNRESET: i32 = 10054;
pub const WSAENOBUFS: i32 = 10055;
pub const WSAEISCONN: i32 = 10056;
pub const WSAENOTCONN: i32 = 10057;
pub const WSAESHUTDOWN: i32 = 10058;
pub const WSAETIMEDOUT: i32 = 10060;
pub const WSAECONNREFUSED: i32 = 10061;
pub const WSAEHOSTUNREACH: i32 = 10065;
pub const WSAEMFILE: i32 = 10024;
const SOCKET_ERROR: i32 = -1;
const WSADATA_SIZE: usize = 408;

static STARTUPS: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static LAST_ERROR: Cell<i32> = const { Cell::new(0) };
}

fn set_error(e: i32) {
    LAST_ERROR.with(|c| c.set(e));
}

pub const INVALID_SOCKET: u64 = u64::MAX;
const SOL_SOCKET_WIN: i32 = 0xffff;
const SOCKADDR_IN_LEN: usize = 16;

/// `errno` of the last call as a Winsock error.
fn wsa_error() -> i32 {
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EINTR) => WSAEINTR,
        Some(libc::EBADF) => WSAENOTSOCK,
        Some(libc::EACCES | libc::EPERM) => WSAEACCES,
        Some(libc::EAGAIN) => WSAEWOULDBLOCK,
        Some(libc::EINPROGRESS) => WSAEINPROGRESS,
        Some(libc::EALREADY) => WSAEALREADY,
        Some(libc::EMSGSIZE) => WSAEMSGSIZE,
        Some(libc::ENOPROTOOPT | libc::EPROTONOSUPPORT) => WSAEPROTONOSUPPORT,
        Some(libc::EOPNOTSUPP) => WSAEOPNOTSUPP,
        Some(libc::EAFNOSUPPORT) => WSAEAFNOSUPPORT,
        Some(libc::EADDRINUSE) => WSAEADDRINUSE,
        Some(libc::EADDRNOTAVAIL) => WSAEADDRNOTAVAIL,
        Some(libc::ENETDOWN) => WSAENETDOWN,
        Some(libc::ENETUNREACH) => WSAENETUNREACH,
        Some(libc::ENETRESET) => WSAENETRESET,
        Some(libc::ECONNABORTED) => WSAECONNABORTED,
        Some(libc::ECONNRESET) => WSAECONNRESET,
        Some(libc::ENOBUFS | libc::ENOMEM) => WSAENOBUFS,
        Some(libc::EISCONN) => WSAEISCONN,
        Some(libc::ENOTCONN) => WSAENOTCONN,
        Some(libc::EPIPE) => WSAESHUTDOWN,
        Some(libc::ETIMEDOUT) => WSAETIMEDOUT,
        Some(libc::ECONNREFUSED) => WSAECONNREFUSED,
        Some(libc::EHOSTUNREACH) => WSAEHOSTUNREACH,
        Some(libc::EMFILE | libc::ENFILE) => WSAEMFILE,
        Some(libc::EFAULT) => WSAEFAULT,
        Some(libc::EINVAL) => WSAEINVAL,
        _ => WSAEINVAL,
    }
}

/// The host descriptor of a Winsock handle (`INVALID_SOCKET` is `WSAENOTSOCK`).
fn fd(s: u64) -> Result<i32, i32> {
    if s == INVALID_SOCKET || s > i32::MAX as u64 {
        return Err(WSAENOTSOCK);
    }
    Ok(s as i32)
}

/// A Win32 socket option as the host's `(level, optname)`.
fn socket_option(level: i32, opt: i32) -> Option<(i32, i32)> {
    Some(match (level, opt) {
        (SOL_SOCKET_WIN, 0x4) => (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        (SOL_SOCKET_WIN, 0x8) => (libc::SOL_SOCKET, libc::SO_KEEPALIVE),
        (SOL_SOCKET_WIN, 0x20) => (libc::SOL_SOCKET, libc::SO_BROADCAST),
        (SOL_SOCKET_WIN, 0x80) => (libc::SOL_SOCKET, libc::SO_LINGER),
        (SOL_SOCKET_WIN, 0x1001) => (libc::SOL_SOCKET, libc::SO_SNDBUF),
        (SOL_SOCKET_WIN, 0x1002) => (libc::SOL_SOCKET, libc::SO_RCVBUF),
        (SOL_SOCKET_WIN, 0x1007) => (libc::SOL_SOCKET, libc::SO_ERROR),
        (libc::IPPROTO_TCP, 0x1) => (libc::IPPROTO_TCP, libc::TCP_NODELAY),
        _ => return None,
    })
}

/// The host's socket address from a Win32 `sockaddr_in` (family, port and address line up).
fn sockaddr(addr: *const u8, len: i32) -> Result<(*const libc::sockaddr, libc::socklen_t), i32> {
    if addr.is_null() || (len as usize) < SOCKADDR_IN_LEN {
        return Err(WSAEFAULT);
    }
    let family = u16::from_ne_bytes(unsafe { [*addr, *addr.add(1)] });
    if family != libc::AF_INET as u16 {
        return Err(WSAEAFNOSUPPORT);
    }
    Ok((addr.cast(), len as libc::socklen_t))
}

/// A Win64 `fd_set`: a 32-bit count, four bytes of alignment, then the 64-bit SOCKETs.
const FD_SET_MAX: usize = 64;
const FD_SET_FIRST: usize = 8;

/// Ordinals of `ws2_32.dll` (stable since Windows 95): `(ordinal, name)`.
pub static ORDINALS: &[(u16, &str)] = &[
    (1, "accept"),
    (2, "bind"),
    (3, "closesocket"),
    (4, "connect"),
    (5, "getpeername"),
    (6, "getsockname"),
    (7, "getsockopt"),
    (8, "htonl"),
    (9, "htons"),
    (10, "ioctlsocket"),
    (11, "inet_addr"),
    (12, "inet_ntoa"),
    (13, "listen"),
    (14, "ntohl"),
    (15, "ntohs"),
    (16, "recv"),
    (17, "recvfrom"),
    (18, "select"),
    (19, "send"),
    (20, "sendto"),
    (21, "setsockopt"),
    (22, "shutdown"),
    (23, "socket"),
    (57, "gethostname"),
    (111, "WSAGetLastError"),
    (112, "WSASetLastError"),
    (115, "WSAStartup"),
    (116, "WSACleanup"),
    (151, "__WSAFDIsSet"),
];

win32! {
    module "ws2_32";
    Native fn WSAStartup(version: u16, data: *mut u8) -> i32 {
        let (major, minor) = ((version & 0xff) as u8, (version >> 8) as u8);
        if major == 0 || major > 2 {
            return WSAVERNOTSUPPORTED;
        }
        if data.is_null() {
            return WSAEINVAL;
        }
        let negotiated: u16 = if major == 2 { 2 | (minor.min(2) as u16) << 8 } else { version };
        std::ptr::write_bytes(data, 0, WSADATA_SIZE);
        *(data as *mut u16) = negotiated;
        *(data.add(2) as *mut u16) = 2 | 2 << 8;
        let desc = b"WinSock 2.0";
        std::ptr::copy_nonoverlapping(desc.as_ptr(), data.add(16), desc.len());
        let status = b"Running";
        std::ptr::copy_nonoverlapping(status.as_ptr(), data.add(16 + 257), status.len());
        STARTUPS.fetch_add(1, Ordering::SeqCst);
        0
    }
    Native fn WSACleanup() -> i32 {
        let ok = STARTUPS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if ok {
            0
        } else {
            set_error(WSANOTINITIALISED);
            SOCKET_ERROR
        }
    }
    // The host name, NUL-terminated, in `name[..len]`; WSAEFAULT when it does not fit.
    Native fn gethostname(name: *mut u8, len: i32) -> i32 {
        if name.is_null() || len < 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let mut buf = [0u8; 256];
        if libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) != 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        buf[255] = 0;
        let n = buf.iter().position(|&b| b == 0).unwrap_or(255);
        if n + 1 > len as usize {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        std::ptr::copy_nonoverlapping(buf.as_ptr(), name, n + 1);
        0
    }
    // No network: every name is unknown. The error is also the return value (not -1).
    Native fn getaddrinfo(_node: *const u8, _service: *const u8, _hints: *const u8, res: *mut *mut u8) -> i32 {
        if !res.is_null() {
            *res = std::ptr::null_mut();
        }
        set_error(WSAHOST_NOT_FOUND);
        WSAHOST_NOT_FOUND
    }
    // Nothing is ever allocated by getaddrinfo, so there is nothing to free.
    Native fn freeaddrinfo(_res: *mut u8) {}
    Native fn WSAGetLastError() -> i32 {
        LAST_ERROR.with(|c| c.get())
    }
    Native fn WSASetLastError(e: i32) {
        set_error(e)
    }

    // -----------------------------------------------------------------------------------
    // Sockets (the host's, behind a Win32 handle; see the module comment).

    Native fn socket(af: i32, ty: i32, protocol: i32) -> u64 {
        if af != libc::AF_INET {
            set_error(WSAEAFNOSUPPORT);
            return INVALID_SOCKET;
        }
        let fd = libc::socket(af, ty, protocol);
        if fd < 0 {
            set_error(wsa_error());
            return INVALID_SOCKET;
        }
        fd as u64
    }
    Native fn closesocket(s: u64) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if libc::close(fd) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn bind(s: u64, addr: *const u8, len: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let (a, l) = match sockaddr(addr, len) {
            Ok(v) => v,
            Err(e) => {
                set_error(e);
                return SOCKET_ERROR;
            }
        };
        if libc::bind(fd, a, l) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn connect(s: u64, addr: *const u8, len: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let (a, l) = match sockaddr(addr, len) {
            Ok(v) => v,
            Err(e) => {
                set_error(e);
                return SOCKET_ERROR;
            }
        };
        if libc::connect(fd, a, l) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn listen(s: u64, backlog: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if libc::listen(fd, backlog.max(1)) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn accept(s: u64, addr: *mut u8, len: *mut i32) -> u64 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return INVALID_SOCKET;
        };
        let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut l = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let c = libc::accept(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut l);
        if c < 0 {
            set_error(wsa_error());
            return INVALID_SOCKET;
        }
        if !addr.is_null() && !len.is_null() && *len >= SOCKADDR_IN_LEN as i32 {
            // the 16 bytes of a sockaddr_in have the same layout as the Win32 one
            std::ptr::copy_nonoverlapping(
                (&a as *const libc::sockaddr_in).cast::<u8>(),
                addr,
                SOCKADDR_IN_LEN,
            );
            *len = SOCKADDR_IN_LEN as i32;
        }
        c as u64
    }
    Native fn getsockname(s: u64, addr: *mut u8, len: *mut i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut l = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::getsockname(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut l) != 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        if addr.is_null() || len.is_null() || *len < SOCKADDR_IN_LEN as i32 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        std::ptr::copy_nonoverlapping(
            (&a as *const libc::sockaddr_in).cast::<u8>(),
            addr,
            SOCKADDR_IN_LEN,
        );
        *len = SOCKADDR_IN_LEN as i32;
        0
    }
    Native fn getpeername(s: u64, addr: *mut u8, len: *mut i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut l = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::getpeername(fd, (&mut a as *mut libc::sockaddr_in).cast(), &mut l) != 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        if addr.is_null() || len.is_null() || *len < SOCKADDR_IN_LEN as i32 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        std::ptr::copy_nonoverlapping(
            (&a as *const libc::sockaddr_in).cast::<u8>(),
            addr,
            SOCKADDR_IN_LEN,
        );
        *len = SOCKADDR_IN_LEN as i32;
        0
    }
    Native fn send(s: u64, buf: *const u8, len: i32, flags: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if buf.is_null() || len < 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let n = libc::send(fd, buf.cast(), len as usize, flags);
        if n < 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        n as i32
    }
    Native fn recv(s: u64, buf: *mut u8, len: i32, flags: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if buf.is_null() || len < 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let n = libc::recv(fd, buf.cast(), len as usize, flags);
        if n < 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        n as i32
    }
    Native fn sendto(
        s: u64,
        buf: *const u8,
        len: i32,
        flags: i32,
        addr: *const u8,
        tolen: i32
    ) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let (a, l) = match sockaddr(addr, tolen) {
            Ok(v) => v,
            Err(e) => {
                set_error(e);
                return SOCKET_ERROR;
            }
        };
        if buf.is_null() || len < 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let n = libc::sendto(fd, buf.cast(), len as usize, flags, a, l);
        if n < 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        n as i32
    }
    Native fn recvfrom(
        s: u64,
        buf: *mut u8,
        len: i32,
        flags: i32,
        addr: *mut u8,
        fromlen: *mut i32
    ) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if buf.is_null() || len < 0 {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut l = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let n = libc::recvfrom(fd, buf.cast(), len as usize, flags, (&mut a as *mut libc::sockaddr_in).cast(), &mut l);
        if n < 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        if !addr.is_null() && !fromlen.is_null() && *fromlen >= SOCKADDR_IN_LEN as i32 {
            std::ptr::copy_nonoverlapping(
                (&a as *const libc::sockaddr_in).cast::<u8>(),
                addr,
                SOCKADDR_IN_LEN,
            );
            *fromlen = SOCKADDR_IN_LEN as i32;
        }
        n as i32
    }
    Native fn shutdown(s: u64, how: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let how = match how {
            0 => libc::SHUT_RD,
            1 => libc::SHUT_WR,
            2 => libc::SHUT_RDWR,
            _ => {
                set_error(WSAEINVAL);
                return SOCKET_ERROR;
            }
        };
        if libc::shutdown(fd, how) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn setsockopt(s: u64, level: i32, opt: i32, value: *const u8, len: i32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let Some((l, o)) = socket_option(level, opt) else {
            set_error(WSAEOPNOTSUPP);
            return SOCKET_ERROR;
        };
        if libc::setsockopt(fd, l, o, value.cast(), len as libc::socklen_t) == 0 {
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    Native fn getsockopt(
        s: u64,
        level: i32,
        opt: i32,
        value: *mut u8,
        len: *mut i32
    ) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        let Some((l, o)) = socket_option(level, opt) else {
            set_error(WSAEOPNOTSUPP);
            return SOCKET_ERROR;
        };
        if len.is_null() {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        let mut l2 = *len as libc::socklen_t;
        if libc::getsockopt(fd, l, o, value.cast(), &mut l2) == 0 {
            *len = l2 as i32;
            0
        } else {
            set_error(wsa_error());
            SOCKET_ERROR
        }
    }
    // `ioctlsocket`: only `FIONBIO` and `FIONREAD` mean anything here.
    Native fn ioctlsocket(s: u64, cmd: i32, arg: *mut u32) -> i32 {
        let Ok(fd) = fd(s) else {
            set_error(WSAENOTSOCK);
            return SOCKET_ERROR;
        };
        if arg.is_null() {
            set_error(WSAEFAULT);
            return SOCKET_ERROR;
        }
        match cmd as u32 {
            // FIONBIO: the host flag from the Win32 value
            0x8004_667e => {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 {
                    set_error(wsa_error());
                    return SOCKET_ERROR;
                }
                let want = *arg != 0;
                let now = flags & libc::O_NONBLOCK != 0;
                if want != now {
                    let f = if want {
                        flags | libc::O_NONBLOCK
                    } else {
                        flags & !libc::O_NONBLOCK
                    };
                    if libc::fcntl(fd, libc::F_SETFL, f) < 0 {
                        set_error(wsa_error());
                        return SOCKET_ERROR;
                    }
                }
                0
            }
            // FIONREAD
            0x4004_667f => {
                let mut n: i32 = 0;
                if libc::ioctl(fd, libc::FIONREAD, &mut n) == 0 {
                    *arg = n.max(0) as u32;
                    0
                } else {
                    set_error(wsa_error());
                    SOCKET_ERROR
                }
            }
            _ => {
                set_error(WSAEINVAL);
                SOCKET_ERROR
            }
        }
    }
    // `select` with Win32 fd sets (count + SOCKET array) and its 8-byte `timeval`.
    Native fn select(
        _nfds: i32,
        readfds: *mut u8,
        writefds: *mut u8,
        exceptfds: *mut u8,
        timeout: *const u8
    ) -> i32 {
        let mut r: libc::fd_set = unsafe { std::mem::zeroed() };
        let mut w: libc::fd_set = unsafe { std::mem::zeroed() };
        let mut e: libc::fd_set = unsafe { std::mem::zeroed() };
        let (mut any, mut max) = (false, 0i32);
        let mut fill = |set: *mut u8, out: &mut libc::fd_set| {
            if set.is_null() {
                return;
            }
            let n = std::ptr::read_unaligned(set as *const u32) as usize;
            for i in 0..n.min(FD_SET_MAX) {
                let s = std::ptr::read_unaligned((set.add(FD_SET_FIRST) as *const u64).add(i));
                let Ok(f) = fd(s) else { continue };
                libc::FD_SET(f, out);
                max = max.max(f);
                any = true;
            }
        };
        fill(readfds, &mut r);
        fill(writefds, &mut w);
        fill(exceptfds, &mut e);
        if !any && timeout.is_null() {
            return 0;
        }
        let tv = if timeout.is_null() {
            std::ptr::null_mut()
        } else {
            // Win32 timeval: two 32-bit longs
            let sec = std::ptr::read_unaligned(timeout as *const i32);
            let usec = std::ptr::read_unaligned((timeout as *const i32).add(1));
            let mut t = libc::timeval {
                tv_sec: sec as libc::time_t,
                tv_usec: usec as libc::suseconds_t,
            };
            (&mut t) as *mut libc::timeval
        };
        let n = libc::select(max + 1, &mut r, &mut w, &mut e, tv);
        if n < 0 {
            set_error(wsa_error());
            return SOCKET_ERROR;
        }
        n
    }
    // `inet_addr`: "a.b.c.d" as the address in network order; `INADDR_NONE` when malformed.
    Native fn inet_addr(cp: *const u8) -> u32 {
        if cp.is_null() {
            set_error(WSAEFAULT);
            return u32::MAX;
        }
        let mut parts = [0u32; 4];
        let mut part = 0usize;
        let mut digits = 0usize;
        let mut value = 0u32;
        let mut i = 0usize;
        loop {
            let b = unsafe { *cp.add(i) };
            i += 1;
            match b {
                b'0'..=b'9' => {
                    value = value.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                    digits += 1;
                    if value > 255 {
                        set_error(WSAEINVAL);
                        return u32::MAX;
                    }
                }
                b'.' if digits > 0 && part < 3 => {
                    parts[part] = value;
                    part += 1;
                    value = 0;
                    digits = 0;
                }
                0 if digits > 0 && part == 3 => {
                    parts[part] = value;
                    // the address in network order, as a word whose bytes in memory are
                    // a.b.c.d (what `sockaddr_in` wants)
                    return u32::from_ne_bytes([
                        parts[0] as u8,
                        parts[1] as u8,
                        parts[2] as u8,
                        parts[3] as u8,
                    ]);
                }
                _ => {
                    set_error(WSAEINVAL);
                    return u32::MAX;
                }
            }
        }
    }
    // `inet_ntoa`: the address as "a.b.c.d" in a per-thread buffer (as Win32 does).
    Native fn inet_ntoa(addr: u32) -> *const u8 {
        thread_local! {
            static NAME: std::cell::RefCell<[u8; 16]> = const { std::cell::RefCell::new([0; 16]) };
        }
        NAME.with(|n| {
            let mut n = n.borrow_mut();
            let a = addr.to_ne_bytes();
            let text = format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
            let bytes = text.as_bytes();
            let len = bytes.len().min(n.len() - 1);
            n[..len].copy_from_slice(&bytes[..len]);
            n[len] = 0;
            n.as_ptr()
        })
    }
    Native fn htonl(v: u32) -> u32 {
        v.swap_bytes()
    }
    Native fn htons(v: u16) -> u16 {
        v.swap_bytes()
    }
    Native fn ntohl(v: u32) -> u32 {
        v.swap_bytes()
    }
    Native fn ntohs(v: u16) -> u16 {
        v.swap_bytes()
    }
    // `__WSAFDIsSet`: nonzero when the socket is in the Win32 fd set.
    Native fn __WSAFDIsSet(s: u64, set: *const u8) -> i32 {
        if set.is_null() {
            return 0;
        }
        let n = std::ptr::read_unaligned(set as *const u32) as usize;
        for i in 0..n.min(FD_SET_MAX) {
            if std::ptr::read_unaligned((set.add(FD_SET_FIRST) as *const u64).add(i)) == s {
                return 1;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lookup, lookup_ordinal};
    use std::ptr::null_mut;

    #[test]
    fn ordinals_resolve_to_the_named_exports() {
        let a = lookup_ordinal("WS2_32.dll", 115).unwrap();
        assert_eq!(a.name, "WSAStartup");
        assert_eq!(a.addr, lookup("ws2_32", "WSAStartup").unwrap().addr);
        assert!(lookup_ordinal("ws2_32.dll", 9999).is_none());
        assert!(lookup_ordinal("kernel32.dll", 115).is_none());
        // the socket calls behind the ordinals are implemented now
        assert_eq!(lookup_ordinal("ws2_32.dll", 23).unwrap().name, "socket");
        assert_eq!(lookup_ordinal("ws2_32.dll", 1).unwrap().name, "accept");
        // an ordinal nobody maps resolves to nothing
        assert!(lookup_ordinal("ws2_32.dll", 9998).is_none());
    }

    #[test]
    fn startup_and_cleanup_balance() {
        unsafe {
            let mut d = [0xAAu8; WSADATA_SIZE];
            assert_eq!(WSAStartup(0x0202, d.as_mut_ptr()), 0);
            assert_eq!(u16::from_le_bytes([d[0], d[1]]), 0x0202);
            assert_eq!(u16::from_le_bytes([d[2], d[3]]), 0x0202);
            assert_eq!(&d[16..27], b"WinSock 2.0");
            assert_eq!(WSAStartup(0x0101, d.as_mut_ptr()), 0);
            assert_eq!(u16::from_le_bytes([d[0], d[1]]), 0x0101);
            assert_eq!(WSAStartup(0x0003, d.as_mut_ptr()), WSAVERNOTSUPPORTED);
            assert_eq!(WSAStartup(0x0202, std::ptr::null_mut()), WSAEINVAL);
            assert_eq!(WSACleanup(), 0);
            assert_eq!(WSACleanup(), 0);
        }
    }

    #[test]
    fn gethostname_fills_the_buffer_or_reports_wsaefault() {
        unsafe {
            let mut buf = [0xAAu8; 300];
            assert_eq!(gethostname(buf.as_mut_ptr(), 300), 0);
            let n = buf.iter().position(|&b| b == 0).unwrap();
            assert!(n > 0);
            // exactly large enough, then one byte too small
            let mut tight = vec![0xAAu8; n + 1];
            assert_eq!(gethostname(tight.as_mut_ptr(), (n + 1) as i32), 0);
            assert_eq!(&tight[..], &buf[..n + 1]);
            WSASetLastError(0);
            assert_eq!(gethostname(tight.as_mut_ptr(), n as i32), SOCKET_ERROR);
            assert_eq!(WSAGetLastError(), WSAEFAULT);
            WSASetLastError(0);
            assert_eq!(gethostname(null_mut(), 16), SOCKET_ERROR);
            assert_eq!(WSAGetLastError(), WSAEFAULT);
            assert_eq!(gethostname(buf.as_mut_ptr(), -1), SOCKET_ERROR);
            assert_eq!(gethostname(buf.as_mut_ptr(), 0), SOCKET_ERROR);
        }
        assert_eq!(
            lookup_ordinal("ws2_32.dll", 57).unwrap().name,
            "gethostname"
        );
    }

    #[test]
    fn getaddrinfo_finds_no_host() {
        unsafe {
            let mut res = 0x1234 as *mut u8;
            let name = b"example.org\0";
            let r = getaddrinfo(name.as_ptr(), null_mut(), std::ptr::null(), &mut res);
            assert_eq!(r, WSAHOST_NOT_FOUND);
            assert!(res.is_null());
            assert_eq!(WSAGetLastError(), WSAHOST_NOT_FOUND);
            assert_eq!(
                getaddrinfo(name.as_ptr(), null_mut(), std::ptr::null(), null_mut()),
                WSAHOST_NOT_FOUND
            );
            freeaddrinfo(res);
        }
    }

    #[test]
    fn a_socket_pair_over_loopback_carries_bytes() {
        unsafe {
            let s = socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert_ne!(s, INVALID_SOCKET, "listen socket: {}", WSAGetLastError());
            let mut addr = [0u8; SOCKADDR_IN_LEN];
            addr[0..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
            addr[2..4].copy_from_slice(&0u16.to_ne_bytes());
            addr[4..8].copy_from_slice(&inet_addr(c"127.0.0.1".as_ptr().cast()).to_ne_bytes());
            assert_eq!(bind(s, addr.as_ptr(), SOCKADDR_IN_LEN as i32), 0);
            assert_eq!(listen(s, 1), 0);
            let mut len = SOCKADDR_IN_LEN as i32;
            assert_eq!(getsockname(s, addr.as_mut_ptr(), &mut len), 0);
            let port = u16::from_ne_bytes([addr[2], addr[3]]);
            assert_ne!(port, 0, "a given port");
            let c = socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert_ne!(c, INVALID_SOCKET);
            assert_eq!(connect(c, addr.as_ptr(), SOCKADDR_IN_LEN as i32), 0);
            let mut peer = [0u8; SOCKADDR_IN_LEN];
            let mut peer_len = SOCKADDR_IN_LEN as i32;
            let accepted = accept(s, peer.as_mut_ptr(), &mut peer_len);
            assert_ne!(accepted, INVALID_SOCKET, "accept: {}", WSAGetLastError());
            let msg = b"hello";
            assert_eq!(send(c, msg.as_ptr(), 5, 0), 5);
            // the accepted socket is readable once the bytes are in (the timeout is 0)
            let mut set = [0u8; FD_SET_FIRST + 8 * FD_SET_MAX];
            set[0..4].copy_from_slice(&1u32.to_ne_bytes());
            set[FD_SET_FIRST..FD_SET_FIRST + 8].copy_from_slice(&accepted.to_ne_bytes());
            assert_eq!(__WSAFDIsSet(accepted, set.as_ptr()), 1);
            let tv = [0u8; 8];
            let n = select(
                0,
                set.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                tv.as_ptr(),
            );
            assert_eq!(n, 1, "the accepted socket is readable");
            let mut got = [0u8; 5];
            assert_eq!(recv(accepted, got.as_mut_ptr(), 5, 0), 5);
            assert_eq!(&got, msg);
            assert_eq!(shutdown(c, 1), 0);
            assert_eq!(closesocket(c), 0);
            assert_eq!(closesocket(accepted), 0);
            assert_eq!(closesocket(s), 0);
        }
    }

    #[test]
    fn a_non_blocking_socket_reports_would_block() {
        unsafe {
            let s = socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            assert_ne!(s, INVALID_SOCKET);
            let mut on: u32 = 1;
            assert_eq!(ioctlsocket(s, 0x8004_667e_u32 as i32, &mut on), 0);
            let mut buf = [0u8; 4];
            assert_eq!(recv(s, buf.as_mut_ptr(), 4, 0), SOCKET_ERROR);
            assert_eq!(WSAGetLastError(), WSAEWOULDBLOCK);
            assert_eq!(closesocket(s), 0);
        }
    }

    #[test]
    fn addresses_and_byte_order_round_trip() {
        unsafe {
            assert_eq!(htons(0x1234), 0x3412);
            assert_eq!(ntohs(0x3412), 0x1234);
            assert_eq!(htonl(0x0102_0304), 0x0403_0201);
            assert_eq!(ntohl(0x0403_0201), 0x0102_0304);
            let a = inet_addr(c"127.0.0.1".as_ptr().cast());
            let name = inet_ntoa(a);
            let text = std::ffi::CStr::from_ptr(name.cast()).to_str().unwrap();
            assert_eq!(text, "127.0.0.1");
            assert_eq!(inet_addr(c"300.0.0.1".as_ptr().cast()), u32::MAX);
            assert_eq!(inet_addr(c"1.2.3".as_ptr().cast()), u32::MAX);
        }
    }

    #[test]
    fn balanced_cleanup_succeeds() {
        unsafe {
            assert_eq!(WSAStartup(0x0202, [0u8; WSADATA_SIZE].as_mut_ptr()), 0);
            assert_eq!(WSACleanup(), 0);
        }
    }

    #[test]
    fn unbalanced_cleanup_sets_the_error() {
        std::thread::spawn(|| unsafe {
            WSASetLastError(0);
            // other tests may hold startups: drain them
            while WSACleanup() == 0 {}
            assert_eq!(WSAGetLastError(), WSANOTINITIALISED);
            WSASetLastError(5);
            assert_eq!(WSAGetLastError(), 5);
        })
        .join()
        .unwrap();
    }
}
