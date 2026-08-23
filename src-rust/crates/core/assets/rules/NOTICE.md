# Third-party notice

Most of the rule files in this directory are adapted from the `oh-my-pi`
project, which is distributed under the MIT License. The frontmatter keys were
translated to this project's shape, the tool names were capitalised, and two
files were rewritten where the original contradicted this codebase. The bodies
are otherwise the original text.

Files adapted from that project:

`go-add-cleanup.md`, `go-exp-promoted.md`, `go-ioutil.md`,
`go-join-hostport.md`, `go-rand-v2.md`, `rs-box-leak.md`,
`rs-future-prelude.md`, `rs-lazylock.md`, `rs-match-ergonomics.md`,
`rs-parking-lot.md`, `rs-result-type.md`, `ts-bare-catch.md`,
`ts-import-type.md`, `ts-no-any.md`, `ts-no-deprecated-leftovers.md`,
`ts-no-dynamic-import.md`, `ts-no-local-is-record.md`, `ts-no-return-type.md`,
`ts-no-test-timers.md`, `ts-no-tiny-functions.md`,
`ts-promise-with-resolvers.md`, `ts-set-map.md`.

Five more are adapted from the same project, but matched a syntax tree there.
Their conditions were rewritten as regular expressions, and each body carries a
note where the new form is looser or narrower than the original:

`go-bench-loop.md`, `go-new-expr.md`, `go-range-int.md`,
`ts-no-inline-cast-access.md`, `ts-redundant-clear-guard.md`.

The rest (`git-add-all.md`, `git-destructive.md`, `no-secrets.md`,
`rs-no-unwrap.md`, `rs-unsafe-safety.md`, `sql-parameterize.md`,
`web-no-localstorage.md`) are this project's own.

## MIT License

Copyright (c) 2025 Mario Zechner
Copyright (c) 2025-2026 Can Bölük
Copyright (c) 2026 Stencil Labs, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
