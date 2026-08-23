---
description: Never pipe a download straight into a shell; save it, read it, then run it
condition:
  - "(curl|wget)[^\\n|]*\\|\\s*(sudo\\s+)?(ba|z|k)?sh\\b"
  - "(curl|wget)[^\\n|]*\\|\\s*(sudo\\s+)?python[0-9.]*\\b"
scope: "tool:Bash, tool:Edit(*.{sh,bash,zsh}), tool:Write(*.{sh,bash,zsh})"
---

`curl … | sh` runs whatever the server sends, with no chance to read it. The
server can serve different content to a pipe than to a browser, and a redirect
or a compromised mirror reaches a shell with your privileges. Worse, a
connection cut halfway leaves a truncated script that has already started
running, so a `rm -rf "$TMP/"` can execute with `$TMP` never assigned.

## Avoid

```bash
curl -fsSL https://example.com/install.sh | sh
wget -qO- https://example.com/setup | sudo bash
```

## Use

```bash
curl -fsSL https://example.com/install.sh -o /tmp/install.sh
# read it
sh /tmp/install.sh
```

Better still, install from the platform's package manager, or verify a checksum
or signature the vendor publishes separately before running anything.
