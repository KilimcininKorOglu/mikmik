---
description: Store browser state in a cookie, not in localStorage
condition:
  - "localStorage"
  - "sessionStorage"
scope: "tool:Edit(*.{js,jsx,ts,tsx,html,svelte,vue}), tool:Write(*.{js,jsx,ts,tsx,html,svelte,vue})"
---

`localStorage` is readable by any script that reaches the page, survives
forever, never reaches the server, and is unavailable in a sandboxed frame or a
browser set to block site data, where the accessor itself throws.

## Use instead

|Need|Use|
|---|---|
|Something the server must see|A cookie, with `HttpOnly` where the page does not read it|
|A session credential|A cookie with `HttpOnly`, `Secure`, `SameSite=Lax`|
|A per-view preference|A cookie, or state held in memory for the page's life|
|Something large|The server, keyed by the session|

```js
// Bad.
localStorage.setItem("theme", "dark");

// Good.
document.cookie = `theme=dark; path=/; max-age=31536000; samesite=lax`;
```

Every read and write of browser storage belongs in a `try`/`catch` whichever
you use: a private window or a blocked-storage setting makes the accessor
throw, and an uncaught throw takes the page down.
