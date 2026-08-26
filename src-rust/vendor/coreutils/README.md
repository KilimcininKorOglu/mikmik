# The coreutils that ship inside the binary

This directory holds a copy of [uutils/coreutils](https://github.com/uutils/coreutils), patched so that a utility's output can be redirected per call.

## Why the copy exists

A model writes `ls`, `cat`, `sort`, `head` and `wc` without asking whether the machine has them. On Windows it usually does not, and on a stripped container image neither does Linux. The published `uu_*` crates solve the availability problem but not the plumbing one: each utility obtains its output with `std::io::stdout()`, which is the process's real standard output. A utility called that way cannot sit in the middle of a pipeline without writing over whatever else the process is printing.

The published crates give no way to redirect that. So the source is copied here and patched: `uucore::streams` holds a per-thread override for stdin, stdout and stderr, and every place a utility obtains one of the three goes through it. `mikmik-shell` then runs a utility on its own thread with the shell's own descriptors in place, which is what makes `ls | sort` stream correctly with no child process at all.

## What was copied

| | |
|---|---|
| Source | The published crates, as they were resolved into `Cargo.lock` |
| `uucore` | 0.8.0, which is what the 82 utilities depend on |
| The 83 `uu_*` crates | 0.8.0. 82 are utilities; `uu_checksum_common` is the macro that generates `uumain` for the checksum ones |
| Size | about 113,000 lines of Rust |

Each directory is the published crate with its `.cargo-checksum.json` removed, which is what lets Cargo treat it as a path dependency. `Cargo.toml.orig` and `LICENSE` are left as they came.

## How Cargo finds them

`src-rust/Cargo.toml` carries a `[patch.crates-io]` table with one entry per crate. `brush-coreutils-builtins` depends on these by version from crates.io, and the table is what points it here instead. Nothing in `brush-coreutils-builtins` is modified.

These are **not** workspace members, on purpose: `cargo clippy --workspace --all-targets` then lints our crates and leaves someone else's code alone.

## What the patch changes

92 files, 1,369 lines added and 380 removed against the published crates. One theme runs through all of it: **a utility here is not a process of its own, so nothing it needs may come from the process.** Upstream took seven things that way.

**1. The three standard streams.** `uucore/src/lib/mods/streams.rs` is new. It holds a per-thread override and hands out stand-ins for `std::io::Stdout`, `Stderr` and `Stdin` that follow it. They answer the same shapes the standard library's do: `Write`, `Read`, `BufRead`, `lock()`, `AsFd`, `AsRawFd`, `is_terminal()`. Every place a utility obtains one of the three now calls `uucore::streams` instead of `std::io`, and a handful of signatures that named `std::io::Stdout` or took `&Stdin` name the stand-in instead.

`is_terminal` is an inherent method rather than an implementation of `std::io::IsTerminal`, because that trait is sealed. The call sites read the same either way.

**2. The exit code.** `uucore::error` kept it in a process-wide static. A utility that reports a partial failure sets it and answers `Ok`, and `uumain` then hands the static back as the exit code. In one process that made one utility's failure the next one's answer, and two pipeline stages running at once clobbered each other's. It is per thread now, and `streams::with_streams` clears it before each run.

**3. The utility's name.** Upstream reads it out of `argv[0]`, which in a host process is the host's binary, so every complaint came out as `mikmik: no-such-file: ...`. The host installs the name for the length of the run and `util_name` answers that.

**4. The message bundle.** Two problems. `uucore`'s `build.rs` walks `src/uu/<name>/locales` to embed each utility's strings, and a vendored checkout has them in a sibling `uu_<name>/locales`, so nothing but `uucore`'s own strings was embedded. And the localizer was set once per thread and cached one resource per role globally, so the second utility on a thread read the first one's bundle. Both are fixed: the build walks the sibling layout, the localizer is rebuilt when the utility changes, and the resource cache is keyed by file. Without this `head` printed `head-error-cannot-open` where it should print `cannot open 'x' for reading`.

**5. The process's signals.** Every `uumain` sets SIGPIPE back to its default on the way in, and clears the SIGSEGV and SIGBUS handlers Rust installs for stack-overflow reporting. Both are right for a standalone utility, which *is* the process: dying on a broken pipe is what `seq inf | head -1` needs. In a host process one `ls` left every later write able to kill MikMik, and took away the message a stack overflow prints for the rest of the process's life. `signals::enable_pipe_errors` and `disable_rust_signal_handlers` now do nothing while a host has streams installed.

**6. The print macros.** Obtaining a stream is only half of it. `print!`, `println!`, `eprint!` and `eprintln!` reach the process's real streams without asking for a handle at all, so 79 call sites across 26 files bypassed everything above. `dirname` writes its whole answer that way, and `du` does too. `uucore::streams` carries four macros of the same names and shapes that go through the override, and each file that prints imports them; a `use` shadows the macro the prelude offers, so the call sites read as they did. The one difference from the standard macros is that a failed write is dropped rather than panicking: in a host process a closed pipe must not end a thread the host owns, and every loop that prints this way is bounded by its input.

**7. The thread the output is written from.** The override is per thread, which holds because a `uumain` is synchronous from start to finish. Two utilities break that on their own: `du` prints its whole answer from a thread it spawns, and `dd` reports its progress from one. Both now carry the streams over with `streams::handoff` and `streams::adopt`. Upstream already knew those threads start blank, and sets the localizer up again inside `dd`'s.

`uu_od` also stops building a `File` from the raw descriptor, which would have closed a descriptor the host is still using.

## What it answers, against GNU

`bundledUtilities` defaults to `prefer`, so this copy stands in for the machine's own `ls` and `sort` rather than only filling a gap. uutils aims at GNU compatibility rather than claiming it, so the difference is measured rather than assumed.

`src-rust/crates/shell/tests/gnu_parity.rs` runs 32 calls a model actually writes, twice on the same input: once through the carried copy, once through the machine's GNU binary, comparing stdout, stderr and exit code. On a machine with GNU coreutils installed, **all 32 answer identically**. A call whose GNU binary is not on the machine skips itself, so the test is useful where GNU is installed and silent where it is not.

Two of the calls found real breaks rather than differences of opinion, and both were the patch's own gap: `dirname` printed to the process's standard output instead of the redirected one, and `du` produced nothing at all because it prints from a thread it spawns. Items 6 and 7 above are what closed them. Nothing is left over that would belong upstream.

The comparison covers the utilities a model reaches for, not the 82. A flag it does not exercise can still differ, and `bundledUtilities: "fallback"` is the way out for a script that depends on one.

## Keeping the patch

The diff is the history rather than a file that can drift from it, but the import commit is not the right base to read it against: it brought `uucore` in at 0.9.0, and the next commit replaced that with the 0.8.0 the 82 utilities depend on. So a `git diff` from the import mixes a version change into the patch.

Measure against the published crates instead, which is where the numbers above come from:

```
diff -ru --exclude=.cargo-checksum.json \
  ~/.cargo/registry/src/*/uucore-0.8.0 src-rust/vendor/coreutils/uucore
```

The same command with a `uu_<name>-0.8.0` pair reads one utility's share of it. `git log --oneline -- src-rust/vendor/coreutils` still says which commit did what.

To bring in a newer release: copy the new crate directories over the old ones, replay that diff, fix what no longer applies, and record the new versions in the table above.

## Licence

uutils/coreutils is MIT. MikMik is GPL-3.0, which MIT is compatible with. Every crate's `LICENSE` file is kept where it was copied to, and the copyright notices are untouched.
