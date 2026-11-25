//! `ws2_32`: Winsock start-up and the per-thread error code (no socket functions yet).
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
const SOCKET_ERROR: i32 = -1;
const WSADATA_SIZE: usize = 408;

static STARTUPS: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static LAST_ERROR: Cell<i32> = const { Cell::new(0) };
}

fn set_error(e: i32) {
    LAST_ERROR.with(|c| c.set(e));
}

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
    Native fn WSAGetLastError() -> i32 {
        LAST_ERROR.with(|c| c.get())
    }
    Native fn WSASetLastError(e: i32) {
        set_error(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lookup, lookup_ordinal};

    #[test]
    fn ordinals_resolve_to_the_named_exports() {
        let a = lookup_ordinal("WS2_32.dll", 115).unwrap();
        assert_eq!(a.name, "WSAStartup");
        assert_eq!(a.addr, lookup("ws2_32", "WSAStartup").unwrap().addr);
        assert!(lookup_ordinal("ws2_32.dll", 9999).is_none());
        assert!(lookup_ordinal("kernel32.dll", 115).is_none());
        // ordinals without an implementation resolve to nothing
        assert!(lookup_ordinal("ws2_32.dll", 23).is_none());
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
