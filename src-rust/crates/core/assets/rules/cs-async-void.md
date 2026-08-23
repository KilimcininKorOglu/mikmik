---
description: Never write async void; return Task so the caller can await the work and see its exceptions
condition: "\\basync\\s+void\\s+\\w+\\s*\\("
scope: "tool:Edit(*.cs), tool:Write(*.cs)"
---

An `async void` method returns nothing to await, so the caller cannot know when
it finished. An exception inside it is raised on the synchronization context
rather than handed to the caller, and on most hosts that ends the process.

## Avoid

```csharp
public async void SaveAsync(Order order)
{
    await _repo.StoreAsync(order);
}
```

## Use

```csharp
public async Task SaveAsync(Order order)
{
    await _repo.StoreAsync(order);
}
```

The caller awaits it, exceptions travel back through the returned task, and a
test can wait for the work to finish.

The one exception is an event handler, whose signature the framework fixes.
Keep its body to a `try`/`catch` around one call to a `Task`-returning method.
