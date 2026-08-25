# mikmik-shell

The shell MikMik runs commands in.

## Why it exists

Every Bash tool call used to spawn `bash -c <script>` and read the shell state back out of a sentinel block printed at the end. That cost a fork and an exec per call, around 15 ms on a laptop before the PTY and the reader thread were counted. It also lost anything a command changed that the sentinel did not name: a shell function, an alias, `shopt`. On Windows there was no bash to spawn, so the tool ran `cmd /C` under a name that promised otherwise.

This crate embeds [brush](https://github.com/reubeno/brush), a bash implementation written in Rust, as a library. One `ShellSession` lives as long as the MikMik session does, so `cd`, `export`, functions and `$?` are the shell's own state. The same code runs on macOS, Linux and Windows.

## What it does not do

An external program is still a real process. brush removes the shell process and its built-ins from the hot path; it does not remove the `cargo`, `git` or `npm` the model asked for.

## Layout

| File | What it holds |
|---|---|
| `src/lib.rs` | `ShellSession`: opening a shell, running one command, the timeout |
| `src/children.rs` | Finding and killing what a timed-out command left running |

## Upstream

| Crate | Version | Licence | What it supplies |
|---|---|---|---|
| `brush-core` | 0.5 | MIT | The parser, the interpreter, redirection, job and process-group handling |
| `brush-parser` | 0.4 | MIT | The tokenizer and the POSIX/bash grammars |
| `brush-builtins` | 0.2 | MIT | The shell built-ins: `cd`, `echo`, `export`, `test`, `printf`, `read`, and the rest |

All three are MIT. MikMik is GPL-3.0, which MIT is compatible with; the upstream copyright notices ship with the dependencies and are not restated in this tree.

brush states that it is not production-complete: `select` and some edge cases are unsupported. `bashEngine: "system"` in `settings.json` puts the session back on the real `bash` binary on Unix, which is why that setting exists.

## Commands

Run from `src-rust/`.

```bash
cargo test --package mikmik-shell -- --test-threads=1
cargo clippy --package mikmik-shell --lib --tests -- -D warnings
```

The tests drive a real shell against a temporary directory: they run pipelines, redirect to files, and time a command out to check that nothing is left running afterwards. Nothing is mocked, because the point of the crate is what the real shell does.
