---
description: Never build a command as one string for Runtime.exec; use ProcessBuilder with an argument list
condition:
  - "Runtime\\.getRuntime\\s*\\(\\s*\\)\\s*\\.exec\\s*\\("
  - "\\.exec\\s*\\(\\s*\"[^\"]*\"\\s*\\+"
scope: "tool:Edit(*.{java,kt,kts}), tool:Write(*.{java,kt,kts})"
---

`Runtime.exec(String)` splits the command on whitespace with a `StringTokenizer`
that knows nothing about quoting. A path with a space becomes two arguments, and
a value from outside the program decides what runs. This is OWASP A03,
injection.

## Avoid

```java
Runtime.getRuntime().exec("convert " + input + " out.png");
```

## Use

```java
ProcessBuilder pb = new ProcessBuilder("convert", "--", input, "out.png");
pb.redirectErrorStream(true);
Process p = pb.start();
int code = p.waitFor();
```

Each argument stays one argument, whatever it contains. `ProcessBuilder` also
lets you set the working directory and the environment, and read both streams
without deadlocking.

Never pass a value that came from outside as the command name itself; match it
against a list of the programs you allow.
