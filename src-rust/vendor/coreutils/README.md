# The coreutils that ship inside the binary

This directory holds a copy of [uutils/coreutils](https://github.com/uutils/coreutils), patched so that a utility's output can be redirected per call.

## Why the copy exists

A model writes `ls`, `cat`, `sort`, `head` and `wc` without asking whether the machine has them. On Windows it usually does not, and on a stripped container image neither does Linux. The published `uu_*` crates solve the availability problem but not the plumbing one: each utility obtains its output with `std::io::stdout()`, which is the process's real standard output. A utility called that way cannot sit in the middle of a pipeline without writing over whatever else the process is printing.

The published crates give no way to redirect that. So the source is copied here and patched: `uucore::streams` holds a per-thread override for stdin, stdout and stderr, and every place a utility obtains one of the three goes through it. `mikmik-shell` then runs a utility on its own thread with the shell's own descriptors in place, which is what makes `ls | sort` stream correctly with no child process at all.

## What was copied

| | |
|---|---|
| Source | The published crates, as they were resolved into `Cargo.lock` |
| `uucore` | 0.9.0 |
| The 83 `uu_*` crates | 0.8.0 |
| Size | 113,298 lines of Rust |

Each directory is the published crate with its `.cargo-checksum.json` removed, which is what lets Cargo treat it as a path dependency. `Cargo.toml.orig` and `LICENSE` are left as they came.

## How Cargo finds them

`src-rust/Cargo.toml` carries a `[patch.crates-io]` table with one entry per crate. `brush-coreutils-builtins` depends on these by version from crates.io, and the table is what points it here instead. Nothing in `brush-coreutils-builtins` is modified.

These are **not** workspace members, on purpose: `cargo clippy --workspace --all-targets` then lints our crates and leaves someone else's code alone.

## Keeping the patch

Every change made here lives as a diff under `patches/`, so a newer uutils release can be brought in and the same changes reapplied rather than rediscovered. The patch surface is about 240 places across 58 crates, not a rewrite of the 113,298 lines.

To bring in a newer release: copy the new crate directories over, apply the patches, fix what no longer applies, and record the new versions in the table above.

## Licence

uutils/coreutils is MIT. MikMik is GPL-3.0, which MIT is compatible with. Every crate's `LICENSE` file is kept where it was copied to, and the copyright notices are untouched.
