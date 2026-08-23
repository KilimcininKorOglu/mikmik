---
description: Never eval a string built from a variable; use an array or a case statement
condition: "\\beval\\s+[\"']?\\$"
scope: "tool:Bash, tool:Edit(*.{sh,bash,zsh}), tool:Write(*.{sh,bash,zsh})"
---

`eval` re-parses its argument as shell source, so every metacharacter in the
value is read as syntax. A variable that came from a file, an environment
variable or a command's output then decides what runs.

## Avoid

```bash
eval "$user_command"
eval "$cmd $args"
```

## Use

For a command with a variable number of arguments, use an array:

```bash
args=(--verbose --output "$out")
mycmd "${args[@]}"
```

For a value that selects behaviour, match it:

```bash
case "$mode" in
  build)  make build ;;
  test)   make test ;;
  *)      echo "unknown mode: $mode" >&2; exit 1 ;;
esac
```

A `case` with an explicit list is both safe and readable, and it fails loudly on
a value nobody planned for.
