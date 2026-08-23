---
description: "Use for i := range n instead of the C-style for i := 0; i < n; i++ loop (Go 1.22)"
condition: "for\\s+\\w+\\s*:=\\s*0\\s*;\\s*\\w+\\s*<[^;]+;\\s*\\w+\\+\\+"
scope: "tool:Edit(*.go), tool:Write(*.go)"
---

Go 1.22: `for` ranges integers. For `i := 0; i < n; i++`, prefer `for i := range n`; if the index is unused, `for range n`.

## Avoid

```go
for i := 0; i < n; i++ {
	use(i)
}

for i := 0; i < len(s); i++ {
	use(s[i])
}
```

## Use

```go
for i := range n {
	use(i)
}

// Ranging the slice directly is usually clearer than indexing.
for i := range s {
	use(s[i])
}

// Index unused, so drop it entirely.
for range n {
	tick()
}
```

## Exceptions

- Keep the explicit form for a non-zero start, a step other than `++`, or a descending loop (`for i := n - 1; i >= 0; i--`). The condition only matches a loop that starts at 0 and steps with `++`, so those never reach you.
- Keep it explicit if the body reassigns the loop variable, or depends on `i` surviving past the loop.
- Requires Go 1.22+. If the module's `go` directive is older, keep the classic loop.
