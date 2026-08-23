---
description: Stage the files you changed by name; never git add -A, --all or .
condition:
  - "git\\s+add\\s+(-A\\b|--all\\b|\\.(\\s|$))"
  - "git\\s+commit\\s+(-a\\b|--all\\b)"
scope: "tool:Bash"
on_match: block
---

`git add -A`, `git add .` and `git commit -a` stage whatever happens to be in
the tree. That is never only your change.

What they sweep in:

- Another agent's work in progress, in a shared checkout or a worktree.
- A file the user was editing and had not finished.
- Build output, a stray log, a scratch file.
- A `.env` or a key that the ignore list did not happen to cover.

None of that is visible in the commit you meant to write, and the first two are
someone else's work landing under your name.

## Instead

```bash
git status --short             # see what is actually there
git add src/a.rs src/b.rs      # name the files this change touches
git commit -m "..."
```

For a change spread over many files, list them; the list is the record of what
the commit is. If the list is long enough to be painful, the commit is probably
several commits.
