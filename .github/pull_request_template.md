## What this adds

<!-- Module and functions, e.g. api-ms-win-crt-string: strcpy_s, strncpy_s, strcat_s -->

Closes #

## Windows behaviour

<!-- Anything non-obvious: where real Windows differs from its documentation or from glibc,
     and your source (docs page, test on Windows, Wine's tests...). -->

## Checklist

- [ ] Each function is declared with `win32!`, under its normalized module name (see `docs/library.md`)
- [ ] `Native` only for functions that really work; anything partial is `Stub`
- [ ] Tests cover normal use **and** error cases (last error, return values, null pointers)
- [ ] Windows types and ABI: `extern "win64"`, 32-bit `LONG`/`long`, UTF-16 wide strings
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` pass locally
- [ ] No game files, console system files, or anything derived from them
- [ ] Code taken from another open-source project is credited and its license allows it
