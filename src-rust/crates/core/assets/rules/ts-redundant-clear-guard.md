---
description: Do not guard clearTimeout, clearInterval or clearImmediate with a truthiness or null check; they accept null and undefined
condition:
  - "if\\s*\\([^)]*\\)\\s*clear(Timeout|Interval|Immediate)\\("
  - "if\\s*\\([^)]*\\)\\s*\\{\\s*clear(Timeout|Interval|Immediate)\\([^)]*\\)\\s*;?\\s*\\}"
scope: "tool:Edit(*.{ts,tsx,js,jsx,mts,cts,mjs,cjs}), tool:Write(*.{ts,tsx,js,jsx,mts,cts,mjs,cjs})"
---

**Do not guard `clearTimeout`, `clearInterval` or `clearImmediate` with a truthiness or `null`/`undefined` check.** Per the WHATWG and Node timers specification, the call does nothing for `null`, `undefined`, or a value with no live timer. The guard cannot change the behaviour. It adds a branch the reader has to reason about, inflates the code, hides the line that matters, and signals that the timer API was misread.

## Avoid

```ts
if (this.timer) clearTimeout(this.timer);
if (handle !== null) clearInterval(handle);
if (id != undefined) {
	clearImmediate(id);
}
```

## Use

```ts
clearTimeout(this.timer);
clearInterval(handle);
clearImmediate(id);
```

## When a guard is warranted

Keep it only when the body does more than clear, such as reassigning the handle or running other cleanup:

```ts
if (this.timer) {
	clearTimeout(this.timer);
	this.timer = undefined; // extra work, so the guard is not purely redundant
}
```

The condition requires the closing brace directly after the clear call, so a guard with extra work in it is left alone.
