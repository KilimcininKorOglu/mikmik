---
description: Never build a command string for system() or popen(); use fork and execv with an argument list
condition: "\\b(system|popen)\\s*\\("
scope: "tool:Edit(*.{c,h,cc,cpp,hpp,cxx}), tool:Write(*.{c,h,cc,cpp,hpp,cxx})"
---

`system` and `popen` pass their argument to `/bin/sh`, which reads `;`, `|`,
`$()` and backticks as syntax. Any part of the string that came from outside
the program then chooses what runs. `system` also runs with the caller's
environment, so a modified `PATH` or `IFS` changes which binary executes. This
is OWASP A03, injection.

## Avoid

```c
char cmd[512];
snprintf(cmd, sizeof cmd, "tar -xzf %s", archive);
system(cmd);
```

## Use

```c
pid_t pid = fork();
if (pid == 0) {
    char *const argv[] = { "tar", "-xzf", "--", (char *)archive, NULL };
    execv("/usr/bin/tar", argv);
    _exit(127);
}
int status;
waitpid(pid, &status, 0);
```

`execv` passes each argument straight through, so no shell reads any of them.
Give the absolute path rather than relying on `PATH`, and put `--` before a
value that could start with `-`.

`posix_spawn` does the same in one call when you do not need to change anything
between the fork and the exec.
