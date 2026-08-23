---
description: Do not throw away uncommitted work with reset --hard, checkout ., clean -fd or stash
condition:
  - "git\\s+reset\\s+--hard"
  - "git\\s+(checkout|restore)\\s+\\.(\\s|$)"
  - "git\\s+clean\\s+-[a-zA-Z]*[fd]"
  - "git\\s+stash(\\s|$)"
  - "git\\s+push\\s+.*--force(-with-lease)?\\b"
  - "git\\s+commit\\s+.*--amend"
scope: "tool:Bash"
on_match: block
---

Each of these destroys work that is not committed, and none of them can be
undone from the shell.

|Command|What is gone|
|---|---|
|`git reset --hard`|Every uncommitted change in the tree|
|`git checkout .` / `git restore .`|The same, file by file|
|`git clean -fd`|Every untracked file, including ones nobody meant to lose|
|`git stash`|Nothing, but it moves work somewhere the user did not put it|
|`git push --force`|A commit somebody else may already have pulled|
|`git commit --amend`|The previous commit, and its hash|

In a shared checkout, or a repository with parallel worktrees, the work you
throw away may not even be yours.

## Instead

- Undo one file: `git checkout -- path/to/file`, named explicitly.
- Undo a commit but keep the work: `git reset --soft HEAD~1`.
- Change the last commit: write a new commit. The history is a record, not a
  draft.
- Remove build output: name it, or use `git clean -n` first and read the list.

If you truly need one of these, say what it will destroy and ask first.
