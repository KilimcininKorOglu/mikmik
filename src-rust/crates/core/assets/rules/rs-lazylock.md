---
description: Keep the initializer with the static; do not hide it in an accessor
condition:
  - "OnceLock::new"
scope: "tool:Edit(*.rs), tool:Write(*.rs)"
---

A lazy static whose initializer is known at declaration time keeps the
initializer next to it. `OnceLock` splits the two: the cell is declared in one
place and filled in another, so a path that reads it before anything filled it
compiles and fails at runtime.

`LazyLock` and `once_cell::sync::Lazy` both hold the cell and the initializer
together. There is no `init()` to call, no repeated `get_or_init`, and no
uninitialized path.

## OnceLock, LazyLock

```rust
// Before, the initializer hides in the accessor.
use std::sync::OnceLock;
static SETTINGS: OnceLock<Settings> = OnceLock::new();
fn settings() -> &'static Settings {
    SETTINGS.get_or_init(Settings::load)
}

// After, the initializer lives with the static.
use std::sync::LazyLock;
static SETTINGS: LazyLock<Settings> = LazyLock::new(Settings::load);
```

`once_cell::sync::Lazy` is the same shape and is what this codebase already
uses for a global registry. Either is right; a bare `OnceLock` with a fixed
initializer is not.

## Keep OnceLock when the value needs runtime input

```rust
use std::sync::OnceLock;
static DATABASE: OnceLock<Database> = OnceLock::new();

fn init_database(url: &str) {
    let _ = DATABASE.set(Database::connect(url));
}
```

The URL is not known at declaration time, so the cell has to be filled later.
That is what `OnceLock` is for.
