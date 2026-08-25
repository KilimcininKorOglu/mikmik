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

Three things, and nothing else:

1. `uucore/src/lib/mods/streams.rs` is new. It holds a per-thread override for the three standard streams and hands out stand-ins for `std::io::Stdout`, `Stderr` and `Stdin` that follow it. They answer the same shapes the standard library's do: `Write`, `Read`, `BufRead`, `lock()`, `AsFd`, `AsRawFd`, `is_terminal()`.
2. Every place a utility obtains one of the three streams calls `uucore::streams` instead of `std::io`. That is 249 lines across 64 files.
3. A handful of signatures that named `std::io::Stdout` or took `&Stdin` now name the stand-in or are generic over the descriptor. `uu_od` also stops building a `File` from the raw descriptor, which would have closed a descriptor the host is still using.

`is_terminal` is an inherent method rather than an implementation of `std::io::IsTerminal`, because that trait is sealed. The call sites read the same either way.

## Keeping the patch

The whole tree is in git, so the diff is the history rather than a file that can drift from it. The import commit brought the source in unchanged; everything after it is the patch:

```
git log --oneline -- src-rust/vendor/coreutils
git diff <import-commit> -- src-rust/vendor/coreutils
```

To bring in a newer release: copy the new crate directories over the old ones, replay that diff, fix what no longer applies, and record the new versions in the table above.

## Licence

uutils/coreutils is MIT. MikMik is GPL-3.0, which MIT is compatible with. Every crate's `LICENSE` file is kept where it was copied to, and the copyright notices are untouched.
