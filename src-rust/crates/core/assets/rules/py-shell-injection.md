---
description: Never run a shell command through a string; pass an argument list and leave shell=True off
condition:
  - "shell\\s*=\\s*True"
  - "os\\.system\\s*\\("
  - "os\\.popen\\s*\\("
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

A command assembled as a string reaches `/bin/sh`, which reads `;`, `|`, `$()`
and backticks. Any value that came from outside the program then chooses what
runs. This is OWASP A03, injection.

## Avoid

```python
os.system(f"tar -xzf {archive}")
subprocess.run(f"grep {pattern} {path}", shell=True)
```

An `archive` of `x.tgz; rm -rf ~` runs both commands.

## Use

```python
subprocess.run(["tar", "-xzf", archive], check=True)
subprocess.run(["grep", "--", pattern, path], check=True)
```

The list form passes each argument straight to `execve`, so no shell reads it.
Add `--` before a value that could start with `-`.

If a pipeline really is needed, build it with two `subprocess.Popen` calls
joined by a pipe, not with a shell string.
