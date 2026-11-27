#!/usr/bin/env python3
"""Drive a native run by keyboard: a uinput virtual keyboard for the titles' menus.

    scripts/vkbd.py [-- <key> ...]        (keys on stdin, one line, space separated)

Creates a uinput keyboard (`/dev/uinput` needs write access; the dsec session's ACL has it),
sends the keys, and destroys it. `NATIV64_WINDOW=wayland`'s keyboard seat maps Enter/Space to A,
Escape/Backspace to B, X/Y to X/Y, the arrow keys to the d-pad and Q/E to the shoulders
(`crates/nativ64-gpu/src/wsi/wayland.rs`, `key_button`), so this is what presses a menu's A
without a controller. Keys are `a` (Space), `enter`, `b` (Escape), `x`, `y`, `up`, `down`,
`left`, `right`, `q`, `e`, or `w<seconds>` to wait.

Example - tick the acknowledgement rows of the terms screen, one second apart:

    echo "w0.5 a w2 down w2 a w2 down w2 a" | scripts/vkbd.py

The keys go to whichever window has the compositor's focus, so bring the title's window up
first; a run without `NATIV64_WINDOW` has no seat and ignores them.
"""

import fcntl
import os
import struct
import sys
import time

UI_DEV_SETUP = 0x405C5503
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502
EV_SYN, EV_KEY = 0, 1
KEYS = {
    "a": 28,          # Enter
    "space": 57,      # Space (also A)
    "enter": 28,
    "b": 1,           # Escape
    "esc": 1,
    "x": 45,
    "y": 21,
    "up": 103,
    "down": 108,
    "left": 105,
    "right": 106,
    "q": 16,
    "e": 18,
}


def create():
    fd = os.open("/dev/uinput", os.O_RDWR | os.O_NONBLOCK)
    fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
    for code in set(KEYS.values()):
        fcntl.ioctl(fd, UI_SET_KEYBIT, code)
    # struct uinput_setup: input_id (4 x u16), name[80], ff_effect_max (u32) = 92 bytes
    setup = struct.pack(
        "<HHHH80sI", 0x03, 0x1234, 0x5678, 0x1, b"nativ64 virtual keyboard", 0
    )
    fcntl.ioctl(fd, UI_DEV_SETUP, setup)
    fcntl.ioctl(fd, UI_DEV_CREATE)
    return fd


def emit(fd, kind, code, value):
    now = time.time()
    # struct input_event: struct timeval (two 8-byte longs on x86-64), type, code, value
    ev = struct.pack("<qqHHi", int(now), int((now % 1) * 1e6), kind, code, value)
    os.write(fd, ev)


def tap(fd, code, hold=0.15):
    emit(fd, EV_KEY, code, 1)
    emit(fd, EV_SYN, 0, 0)
    time.sleep(hold)
    emit(fd, EV_KEY, code, 0)
    emit(fd, EV_SYN, 0, 0)
    time.sleep(0.12)


def main():
    text = sys.stdin.read()
    fd = create()
    try:
        time.sleep(0.3)
        for arg in text.split():
            if arg.startswith("w") and arg[1:].replace(".", "", 1).isdigit():
                time.sleep(float(arg[1:]))
            elif arg in KEYS:
                tap(fd, KEYS[arg])
            else:
                print(f"unknown key {arg!r}", file=sys.stderr)
                return 2
    finally:
        time.sleep(0.2)
        fcntl.ioctl(fd, UI_DEV_DESTROY)
        os.close(fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
