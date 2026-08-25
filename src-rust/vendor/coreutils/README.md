# The coreutils that ship inside the binary

This directory holds a copy of [uutils/coreutils](https://github.com/uutils/coreutils), patched so that a utility's output can be redirected per call.

## Why the copy exists

A model writes `ls`, `cat`, `sort`, `head` and `wc` without asking whether the machine has them. On Windows it usually does not, and on a stripped container image neither does Linux. The published `uu_*` crates solve the availability problem but not the plumbing one: each utility obtains its output with `std::io::stdout()`, which is the process's real standard output. A utility called that way cannot sit in the middle of a pipeline without writing over whatever else the process is printing.

The published crates give no way to redirect that. So the source is copied here and patched: `uucore::streams` holds a per-thread override for stdin, stdout and stderr, and every place a utility obtains one of the three goes through it. `mikmik-shell` then runs a utility on its own thread with the shell's own descriptors in place, which is what makes `ls | sort` stream correctly with no child process at all.

## What was copied

| | |
|---|---|
| Source | The published crates, as they were resolved into `Cargo.lock` |
| `uucore` | 0.8.0, which is what the 83 utilities depend on |
| The 83 `uu_*` crates | 0.8.0 |
| Size | about 113,000 lines of Rust |

Each directory is the published crate with its `.cargo-checksum.json` removed, which is what lets Cargo treat it as a path dependency. `Cargo.toml.orig` and `LICENSE` are left as they came.

## How Cargo finds them

`src-rust/Cargo.toml` carries a `[patch.crates-io]` table with one entry per crate. `brush-coreutils-builtins` depends on these by version from crates.io, and the table is what points it here instead. Nothing in `brush-coreutils-builtins` is modified.

These are **not** workspace members, on purpose: `cargo clippy --workspace --all-targets` then lints our crates and leaves someone else's code alone.

## What the patch changes

84 files, 509 lines added and 375 removed against the published crates. One theme runs through all of it: **a utility here is not a process of its own, so nothing it needs may come from the process.** Upstream took four things that way.

**1. The three standard streams.** `uucore/src/lib/mods/streams.rs` is new. It holds a per-thread override and hands out stand-ins for `std::io::Stdout`, `Stderr` and `Stdin` that follow it. They answer the same shapes the standard library's do: `Write`, `Read`, `BufRead`, `lock()`, `AsFd`, `AsRawFd`, `is_terminal()`. Every place a utility obtains one of the three now calls `uucore::streams` instead of `std::io`, and a handful of signatures that named `std::io::Stdout` or took `&Stdin` name the stand-in instead.

`is_terminal` is an inherent method rather than an implementation of `std::io::IsTerminal`, because that trait is sealed. The call sites read the same either way.

**2. The exit code.** `uucore::error` kept it in a process-wide static. A utility that reports a partial failure sets it and answers `Ok`, and `uumain` then hands the static back as the exit code. In one process that made one utility's failure the next one's answer, and two pipeline stages running at once clobbered each other's. It is per thread now, and `streams::with_streams` clears it before each run.

**3. The utility's name.** Upstream reads it out of `argv[0]`, which in a host process is the host's binary, so every complaint came out as `mikmik: no-such-file: ...`. The host installs the name for the length of the run and `util_name` answers that.

**4. The message bundle.** Two problems. `uucore`'s `build.rs` walks `src/uu/<name>/locales` to embed each utility's strings, and a vendored checkout has them in a sibling `uu_<name>/locales`, so nothing but `uucore`'s own strings was embedded. And the localizer was set once per thread and cached one resource per role globally, so the second utility on a thread read the first one's bundle. Both are fixed: the build walks the sibling layout, the localizer is rebuilt when the utility changes, and the resource cache is keyed by file. Without this `head` printed `head-error-cannot-open` where it should print `cannot open 'x' for reading`.

`uu_od` also stops building a `File` from the raw descriptor, which would have closed a descriptor the host is still using.

## Keeping the patch

The whole tree is in git, so the diff is the history rather than a file that can drift from it. The import commit brought the source in unchanged; everything after it is the patch:

```
git log --oneline -- src-rust/vendor/coreutils
git diff <import-commit> -- src-rust/vendor/coreutils
```

To bring in a newer release: copy the new crate directories over the old ones, replay that diff, fix what no longer applies, and record the new versions in the table above.

## Licence

uutils/coreutils is MIT. MikMik is GPL-3.0, which MIT is compatible with. Every crate's `LICENSE` file is kept where it was copied to, and the copyright notices are untouched.
