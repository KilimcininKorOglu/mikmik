---
description: "Use new(expr) for pointer-to-value helpers instead of func ptr[T any](v T) *T { return &v } (Go 1.26)"
condition: "func\\s+\\w+(\\[[^\\]]*\\])?\\(\\s*\\w+\\s+[\\w.\\[\\]]+\\s*\\)\\s*\\*[\\w.\\[\\]]+\\s*\\{\\s*return\\s+&\\w+\\s*\\}"
scope: "tool:Edit(*.go), tool:Write(*.go)"
---

Go 1.26: `new(expr)` allocates, stores `expr` and returns `*T`. It replaces pointer-value helpers and the `x := v; p := &x` dance.

## Why

- Replaces the per-type helpers (`boolPtr`, `strPtr`, `int64Ptr`, and the rest) and `func Ptr[T any](v T) *T`.
- The value is constructed directly in the allocation: no extra call frame, no separate heap escape.
- The intent is visible at the call site: `new(false)`, not a helper name.

## Avoid

```go
// A helper that just takes a value and returns its address.
func boolPtr(v bool) *bool     { return &v }
func strPtr(v string) *string  { return &v }
func Ptr[T any](v T) *T        { return &v }

cfg := Config{Enabled: boolPtr(true), Name: strPtr("svc")}
```

## Use

```go
cfg := Config{Enabled: new(true), Name: new("svc")}

// Was: x := int64(300); p := &x
p := new(int64(300))
```

`new(true)` and `new(false)` give `*bool`. `new(expr)` takes any expression, a function result included (`new(time.Now())`).

## Notes

- Requires Go 1.26+. If the module's `go` directive is older, keep the helper or the temporary variable until the toolchain is bumped.
- The condition matches a function whose whole body is `return &x`. A function that does work before taking an address is a different thing, and is left alone.
- `new(T)` on a bare type is unchanged and still zero-initialises.
