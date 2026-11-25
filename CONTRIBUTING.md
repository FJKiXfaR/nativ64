# Contributing

Thanks for helping. Everything happens on GitHub: issues for questions and claims, pull
requests for code.

## Pick something

1. Open the import map (`docs/import-map.md` explains how to draw it) or browse the
   issues labelled `good first issue`.
2. Comment on the issue, or open one, naming the functions you take, so two people
   don't write the same thing.
3. Prefer a coherent group (say, `api-ms-win-crt-string` `strncpy_s`/`strcpy_s`/`strcat_s`)
   over scattered functions.

## Write it

- One Rust module per Windows module, under `crates/vn-win32/src/`. A function is
  declared and registered in one place (see `docs/library.md`).
- Match Windows, not glibc: Windows x64 calling convention (`extern "win64"`), `LONG` is
  32 bits, wide strings are UTF-16, failures set the last error. When Windows
  documentation and real behaviour disagree, follow real behaviour and say so in a
  comment with your source.
- A function that only pretends to work is registered as `Stub`, never `Native`.
- Every `Native` function has tests, including its error cases.

## Before opening a pull request

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

CI runs the same checks.

## Rules

- **Never commit game files, console system files, or anything extracted or derived from
  them** (dumps, import lists, logs, screenshots of the game). The only exception is the
  aggregate coverage chart in `docs/`. Pull requests containing such files are closed.
- Reimplement from public documentation and observed behaviour. Do not paste code from
  leaked or proprietary sources. Code from other open-source projects is welcome when
  its license allows it: credit it in the file.
- English for code, comments, commits and issues.

By contributing, you agree that your work is licensed under MIT or Apache-2.0, like the
rest of the project.
