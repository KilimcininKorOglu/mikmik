---
description: Do not use .unwrap() or .expect() on a fallible operation in production code
condition:
  - "\\.unwrap\\(\\)"
  - "\\.expect\\("
scope: "tool:Edit(*.rs), tool:Write(*.rs)"
---

`.unwrap()` and `.expect()` turn a recoverable failure into a panic that takes
the whole process with it. A library that panics gives its caller no way to
report the problem, retry, or clean up.

## Use instead

|Situation|Reach for|
|---|---|
|The caller can act on the failure|`?` with a `Result` return type|
|A missing value is ordinary|`if let Some(v) = ...` or `let Some(v) = ... else`|
|A default is correct|`unwrap_or`, `unwrap_or_else`, `unwrap_or_default`|
|The failure needs context|`map_err`, or `anyhow::Context::context`|

```rust
// Bad, the process dies on a file the user simply does not have.
let text = std::fs::read_to_string(path).unwrap();

// Good, the caller decides.
let text = std::fs::read_to_string(path)
    .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
```

## Where it is fine

- Test code. A panic **is** the failure report there.
- A `Mutex` lock, where the alternative is a poisoned lock nobody can recover
  from. Prefer `parking_lot`, whose `lock()` does not return a `Result` at all.
- A value the type system cannot prove but the code just constructed, with a
  comment saying why it cannot fail.

If you are in test code, this rule does not apply and you can ignore it.
