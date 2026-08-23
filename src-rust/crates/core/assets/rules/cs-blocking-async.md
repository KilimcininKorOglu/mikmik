---
description: Never block on a task with .Result or .Wait(); await it
condition:
  - "\\.Wait\\s*\\(\\s*\\)"
  - "\\.GetAwaiter\\s*\\(\\s*\\)\\s*\\.GetResult\\s*\\("
  - "\\.Result\\s*;"
scope: "tool:Edit(*.cs), tool:Write(*.cs)"
---

Blocking on a task holds the current thread while the continuation waits for
that same thread. On any host with a synchronization context (ASP.NET, WPF,
WinForms) that is a deadlock the debugger shows as a hang with no exception.
Even where it does not deadlock, it wastes a thread-pool thread and wraps any
exception in an `AggregateException`.

## Avoid

```csharp
var order = _repo.LoadAsync(id).Result;
_repo.SaveAsync(order).Wait();
```

## Use

```csharp
var order = await _repo.LoadAsync(id);
await _repo.SaveAsync(order);
```

Make the calling method `async Task` and let `await` travel up. An entry point
can be `static async Task Main`.

In a library, add `.ConfigureAwait(false)` to every await, so the continuation
does not need the caller's context at all.
