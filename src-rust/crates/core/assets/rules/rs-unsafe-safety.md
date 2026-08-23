---
description: Every unsafe block carries a // SAFETY: comment saying why it is sound
condition:
  - "unsafe\\s*\\{"
  - "unsafe\\s+(fn|impl|extern)"
scope: "tool:Edit(*.rs), tool:Write(*.rs)"
---

An `unsafe` block is a promise to the compiler that you checked what it cannot.
The promise is worthless if the next reader cannot tell what was checked.

Put a `// SAFETY:` comment directly above the block. Say which invariant holds
and why, not what the code does.

```rust
// Bad, the promise is invisible.
unsafe { std::ptr::write(dst, value) }

// Good.
// SAFETY: `dst` came from `Vec::as_mut_ptr` above and `index < len` was
// checked, so the pointer is aligned, in bounds, and uniquely owned here.
unsafe { std::ptr::write(dst.add(index), value) }
```

An `unsafe fn` carries the same comment on its declaration, naming what the
caller has to guarantee.

## Before reaching for unsafe

Most `unsafe` in ordinary code is avoidable:

|Reason it looked necessary|Safe answer|
|---|---|
|Two mutable references into one slice|`split_at_mut`, `chunks_mut`, `iter_mut`|
|A `'static` lifetime you cannot produce|Owned data, `Arc<T>`, or `LazyLock<T>`|
|Skipping a bounds check for speed|Measure first; the check is usually free|
|Transmuting between types|`bytemuck`, `TryFrom`, or an explicit conversion|

If none of those fit, write the block and write the comment.
