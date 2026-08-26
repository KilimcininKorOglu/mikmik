# mikmik-shell

The shell MikMik runs commands in.

## Why it exists

Every Bash tool call used to spawn `bash -c <script>` and read the shell state back out of a sentinel block printed at the end. That cost a fork and an exec per call, 3.14 ms on this laptop before the PTY and the reader thread were counted. It also lost anything a command changed that the sentinel did not name: a shell function, an alias, `shopt`. On Windows there was no bash to spawn, so the tool ran `cmd /C` under a name that promised otherwise.

This crate embeds [brush](https://github.com/reubeno/brush), a bash implementation written in Rust, as a library. One `ShellSession` lives as long as the MikMik session does, so `cd`, `export`, functions and `$?` are the shell's own state. The same code runs on macOS, Linux and Windows.

## Bundled utilities

A model writes `ls`, `cat`, `sort`, `head`, `wc`, `sed`, `find` and `jq` without asking whether the machine has them. On Windows it usually does not, and on a stripped container image neither does Linux. 83 `uutils` coreutils plus `find`, `xargs`, `sed` and `jq` are compiled into the binary.

**They run in this process.** Nothing is spawned. The published `uutils` crates could not manage that, because each obtains its output with `std::io::stdout()`, so a utility called that way writes over whatever else the process is printing. The source is forked under `vendor/coreutils/` and patched: `uucore::streams` holds a per-thread override for the three standard streams, and each utility runs on its own thread with the shell's descriptors installed for that thread. `vendor/coreutils/README.md` has the whole of it.

**Which copy wins is a setting.** `BundledUtilities::Prefer` registers every bundled name and is the default, because the bundled copy is in this process and the machine's costs a fork and an exec. `Fallback` registers only a name `which` cannot find, which leaves a Unix box with GNU coreutils behaving exactly as it did. brush's own built-ins win either way, so `echo`, `printf`, `test`, `true` and `false` keep the shell's semantics rather than the coreutils ones.

`find` and `jq` take a shorter path: `findutils` and `jaq` are libraries that write through a writer the caller supplies, so neither needs a stream override at all.

## What it cost

Measured on an Apple laptop with `examples/run_commands`, release build. Each command runs in one long-lived session, so the figures are what the second and later calls cost.

| | `bash -c` | Bundled, in process | The machine's own binary |
|---|---|---|---|
| One trivial command | 3.14 ms (`bash -c true`) | 34 µs | — |
| `ls -1` | — | 164 µs | 2.54 ms |
| `sort list` | — | 180 µs | 2.32 ms |
| `seq 1 200000 \| sort -n \| tail -1` | — | 12.5 ms | — |

The first call to a given utility costs more than the ones after it: 6.48 ms for the first `ls`, against 223 µs for the second. That is the utility's message bundle being built once per thread.

| | Before | After |
|---|---|---|
| Release binary | 35.3 MiB | 52.1 MiB |
| Cold build of the 83 `uu_*` crates | — | 17 min 40 s, once |
| Warm `cargo check --workspace` | 17 s | 17 s |

The 16.8 MiB is what the bundled utilities weigh. The compile cost is paid once per machine and then cached; a warm check is unchanged.

The per-command figure was almost lost to the safeguard rather than the shell. Listing this process's children before each command through `pgrep -P` cost 27 ms, which is why `children.rs` asks the platform directly instead.

## How the shell is configured

`ShellSession::new` builds a shell that is deliberately not interactive, and skips both the profile and the rc files. What that turns off, and why each stays off:

| Off | What it means | Why |
|---|---|---|
| The profile and rc files | No `~/.bashrc`, no `~/.bash_profile`, no `BASH_ENV` | They name files the user controls, and sourcing one would run whatever is in it before every command the model writes. `bash -c` did not read them either |
| `enable_job_control` | `&` prints the command's output and nothing else | Matches `bash -c`. A `[1]+ Done` line brush added would read as part of the command's output. A script that wants job control writes `set -m`, which works |
| `enable_command_history` | No history file | Nothing reads it |
| `enable_bang_style_history_substitution` | `!!` and `!$` do not expand | There is no history to expand from |
| `emacs_mode` | No line editor | Nothing is typed at this shell |
| `PS1` and `PS2` | Not set | A prompt string cannot reach a tool's output |

Two things a reader might assume it takes away, and it does not. **Aliases expand**: `alias ll='ls -l'; ll` works, even though `expand_aliases` is one of the options brush turns on for interactive shells. And **a fatal error is a status rather than the end of the session**: `set -u` followed by an unset variable answers non-zero and the session runs the next command.

Job control being off is also why `run` sets `NewProcessGroup` explicitly. `default_exec_params` answers `SameProcessGroup` while job control is off, and a timeout that killed *that* group would kill this process with it.

## Background commands

`run_in_background` opens a **second** `ShellSession`, seeded from the foreground one's working directory and exported variables. That is what `&` does in bash: the command sees the session's state, and what it changes does not come back. It also means a background command runs while a foreground one does, which sharing the session could not manage.

`monitor cancel` reaches it through a cancellation token rather than a process id. The shell is this process, so there is no pid to signal; the token ends the wait and kills whatever the command started, exactly as the timeout does.

Starting one never waits for the command. Reading the seed does take the foreground session's lock, so a background command started while a foreground one is in flight begins when that one ends; the tool call itself answers straight away with the task's id.

## What running in this process costs

Three things a child process gave for free.

**A utility cannot be killed.** A timeout stops the caller waiting for it; the thread runs to the end. `run` answers 124 and kills the *processes* the command started, and a bundled utility is not one of them. `sleep 30` starts nothing, so nothing is left for a timeout or a cancel to reach: a test about killing a child has to spell out `/bin/sleep`.

**The working directory is the process's.** brush keeps the shell's own and hands it to a child through `Command::current_dir`; a utility running here resolves `sort list` against whatever directory the process is in. `src/cwd.rs` lends the process's directory for the length of the call, shared by everything asking for the same one so a pipeline cannot deadlock on it, and put back afterwards.

**State that used to be per process is now per thread.** The exit code, the utility's name and its message bundle all came from the utility being a process of its own. Each is scoped to one run in the fork; the patch notes say how. The same applies to the process's signal dispositions, which every `uumain` used to reset: one `ls` left MikMik able to be killed by any later broken-pipe write.

## What it does not do

An external program is still a real process. brush removes the shell process and its built-ins from the hot path; it does not remove the `cargo`, `git` or `npm` the model asked for.

## Layout

| File | What it holds |
|---|---|
| `src/lib.rs` | `ShellSession`: opening a shell, running one command, the timeout |
| `src/children.rs` | Finding and killing what a timed-out command left running |
| `src/cwd.rs` | Lending the process's working directory to a bundled utility |
| `src/streams.rs` | Installing the shell's descriptors for one utility's run |
| `src/bundled.rs` | The registry of bundled commands and the built-in that runs one |
| `src/bundled/find.rs` | `find`, through the `findutils` library |
| `src/bundled/jq.rs` | `jq`, through the `jaq` libraries |

## Upstream

| Crate | Version | Licence | What it supplies |
|---|---|---|---|
| `brush-core` | 0.5 | MIT | The parser, the interpreter, redirection, job and process-group handling |
| `brush-parser` | 0.4 | MIT | The tokenizer and the POSIX/bash grammars |
| `brush-builtins` | 0.2 | MIT | The shell built-ins: `cd`, `echo`, `export`, `test`, `printf`, `read`, and the rest |
| `brush-coreutils-builtins` | 0.1 | MIT | The registry of 83 `uutils` coreutils, behind the `coreutils.all` feature |
| `uucore` and 83 `uu_*` | 0.8 | MIT | The coreutils themselves, forked under `vendor/coreutils/` |
| `findutils` | 0.10 | MIT | `find` and `xargs` |
| `sed` | 0.1 | MIT | `sed` |
| `jaq-core`, `jaq-std`, `jaq-json` | 3.1, 3.0, 2.0 | MIT | The `jq` filter language, its standard library and its JSON built-ins |

All are MIT. MikMik is GPL-3.0, which MIT is compatible with. The forked coreutils keep their own `LICENSE` files and copyright notices where they were copied to; the rest ship their notices with the dependencies.

`brush-interactive` is not taken. It supplies the readline layer, the prompt, the completion menu and the line editor, and MikMik's TUI already owns all of them. A session here is driven by a model rather than typed at, so there is no line to edit, no prompt to draw and no completion to offer. Taking it would also mean turning the shell interactive, which is what the table above spends its length arguing against.

brush states that it is not production-complete: `select` and some edge cases are unsupported. `bashEngine: "system"` in `settings.json` puts the session back on the real `bash` binary on Unix, which is why that setting exists.

## Commands

Run from `src-rust/`.

```bash
cargo test --package mikmik-shell -- --test-threads=1
cargo clippy --package mikmik-shell --lib --tests -- -D warnings
```

The tests drive a real shell against a temporary directory: they run pipelines, redirect to files, and time a command out to check that nothing is left running afterwards. Nothing is mocked, because the point of the crate is what the real shell does.
