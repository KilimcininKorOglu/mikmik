# mikmik-shell

The shell MikMik runs commands in.

## Why it exists

Every Bash tool call used to spawn `bash -c <script>` and read the shell state back out of a sentinel block printed at the end. That cost a fork and an exec per call, around 15 ms on a laptop before the PTY and the reader thread were counted. It also lost anything a command changed that the sentinel did not name: a shell function, an alias, `shopt`. On Windows there was no bash to spawn, so the tool ran `cmd /C` under a name that promised otherwise.

This crate embeds [brush](https://github.com/reubeno/brush), a bash implementation written in Rust, as a library. One `ShellSession` lives as long as the MikMik session does, so `cd`, `export`, functions and `$?` are the shell's own state. The same code runs on macOS, Linux and Windows.

## Bundled utilities

A model writes `ls`, `cat`, `sort`, `head`, `wc`, `sed`, `find` and `jq` without asking whether the machine has them. On Windows it usually does not, and on a stripped container image neither does Linux. Around eighty `uutils` coreutils plus `find`, `xargs`, `sed` and `jq` are compiled into the binary.

**The machine's own binary wins.** A shim is registered only for a name `which` cannot find on `PATH`. On a Unix box with GNU coreutils installed nothing bundled is ever reached, so behaviour there is unchanged. The bundled set is what Windows and a bare container get.

**In the binary is not the same as in the process.** A `uutils` utility writes to the process's real standard output and ends by calling `std::process::exit`, so it cannot be called in the middle of a pipeline without taking MikMik with it. The shim therefore re-executes this binary as `mikmik --invoke-bundled <name> <args>`, which upstream brush does the same way. Redirection, pipes and process groups then work because the child is an ordinary process. What the bundled set removes is the need to install anything; it does not remove the fork and exec.

`find` and `jq` are the exceptions. `findutils` and `jaq` are libraries that write through a writer the caller supplies, so both run in this process with no child at all.

The dispatch flag counts in the first argument position only. `echo --invoke-bundled ls` stays an `echo`.

## What it cost

Measured on an Apple laptop with `examples/shim_check`, release build.

| | Before | After |
|---|---|---|
| One trivial command | 3.14 ms (`bash -c true`) | 34 µs |
| Release binary | 35.3 MiB | 51.8 MiB |
| Cold build of the 83 `uu_*` crates | — | 17 min 40 s, once |
| Warm `cargo check --workspace` | 17 s | 17 s |

The 16.5 MiB is what the bundled utilities weigh. The compile cost is paid once per machine and then cached; a warm check is unchanged.

The per-command figure was almost lost to the safeguard rather than the shell. Listing this process's children before each command through `pgrep -P` cost 27 ms, which is why `children.rs` asks the platform directly instead.

## What it does not do

An external program is still a real process. brush removes the shell process and its built-ins from the hot path; it does not remove the `cargo`, `git` or `npm` the model asked for.

## Layout

| File | What it holds |
|---|---|
| `src/lib.rs` | `ShellSession`: opening a shell, running one command, the timeout |
| `src/children.rs` | Finding and killing what a timed-out command left running |
| `src/bundled.rs` | The registry of bundled commands, the shim built-in, and `--invoke-bundled` |
| `src/bundled/find.rs` | `find`, running in this process |
| `src/bundled/jq.rs` | `jq`, running in this process |

## Upstream

| Crate | Version | Licence | What it supplies |
|---|---|---|---|
| `brush-core` | 0.5 | MIT | The parser, the interpreter, redirection, job and process-group handling |
| `brush-parser` | 0.4 | MIT | The tokenizer and the POSIX/bash grammars |
| `brush-builtins` | 0.2 | MIT | The shell built-ins: `cd`, `echo`, `export`, `test`, `printf`, `read`, and the rest |
| `brush-coreutils-builtins` | 0.1 | MIT | Around eighty `uutils` coreutils, behind the `coreutils.all` feature |
| `findutils` | 0.10 | MIT | `find` and `xargs` |
| `sed` | 0.1 | MIT | `sed` |
| `jaq-core`, `jaq-std`, `jaq-json` | 3.1, 3.0, 2.0 | MIT | The `jq` filter language, its standard library and its JSON built-ins |

All are MIT. MikMik is GPL-3.0, which MIT is compatible with; the upstream copyright notices ship with the dependencies and are not restated in this tree. The dispatch protocol and the shim built-in in `src/bundled.rs` are adapted from `brush-shell/src/bundled.rs` by Reuben Olinsky, and the module names him.

brush states that it is not production-complete: `select` and some edge cases are unsupported. `bashEngine: "system"` in `settings.json` puts the session back on the real `bash` binary on Unix, which is why that setting exists.

## Commands

Run from `src-rust/`.

```bash
cargo test --package mikmik-shell -- --test-threads=1
cargo clippy --package mikmik-shell --lib --tests -- -D warnings
```

The tests drive a real shell against a temporary directory: they run pipelines, redirect to files, and time a command out to check that nothing is left running afterwards. Nothing is mocked, because the point of the crate is what the real shell does.
