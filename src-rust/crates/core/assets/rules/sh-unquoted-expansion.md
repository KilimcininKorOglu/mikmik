---
description: Quote every variable expansion; an unquoted one splits on whitespace and expands globs
condition:
  - "\\brm\\s+(-[a-zA-Z-]+\\s+)*\\$[\\w{]"
  - "\\bcd\\s+\\$[\\w{]"
  - "\\bmv\\s+\\$[\\w{]"
scope: "tool:Bash, tool:Edit(*.{sh,bash,zsh}), tool:Write(*.{sh,bash,zsh})"
---

An unquoted `$VAR` goes through word splitting and then glob expansion. A path
with a space becomes two arguments, and a `*` in the value matches files in the
current directory. On `rm`, `cd` and `mv` that turns a wrong value into lost
work.

## Avoid

```bash
rm -rf $BUILD_DIR/*      # BUILD_DIR unset expands to `rm -rf /*`
cd $project && make
mv $src $dst
```

## Use

```bash
rm -rf "${BUILD_DIR:?BUILD_DIR is not set}"/*
cd "$project" && make
mv -- "$src" "$dst"
```

`${VAR:?message}` stops the script when the variable is empty or unset, which
is exactly the case that makes an unquoted `rm` dangerous. Put `--` before a
value that could start with `-`.

`set -euo pipefail` at the top of the script catches the rest.
