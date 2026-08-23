---
description: "Use for b.Loop() in benchmarks instead of the for i := 0; i < b.N; i++ loop (Go 1.24)"
condition: "<\\s*\\w+\\.N\\b"
scope: "tool:Edit(*_test.go), tool:Write(*_test.go)"
---

Go 1.24 added `testing.B.Loop`. Write `for b.Loop() { ... }` instead of looping over `b.N`.

## Why

- Setup and teardown outside the loop run exactly once per `-count`, not once per `b.N` re-estimation, so an expensive fixture is no longer timed or repeated.
- The compiler keeps the loop's parameters and results alive, so it cannot optimise away the body you are trying to measure. That is the classic `b.N` benchmarking trap.

## Avoid

```go
func BenchmarkEncode(b *testing.B) {
	for i := 0; i < b.N; i++ {
		Encode(input)
	}
}
```

## Use

```go
func BenchmarkEncode(b *testing.B) {
	for b.Loop() {
		Encode(input)
	}
}
```

Requires Go 1.24+. If the module targets an older Go, keep the `b.N` loop.
