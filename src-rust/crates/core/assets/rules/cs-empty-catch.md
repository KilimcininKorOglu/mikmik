---
description: Never leave a catch block empty, and never catch Exception only to hide it
condition: "catch\\s*(\\([^)]*\\))?\\s*\\{\\s*\\}"
scope: "tool:Edit(*.cs), tool:Write(*.cs)"
---

An empty catch turns a failure into silence. The method returns as if it
succeeded, and the wrong value travels on until something unrelated breaks.

## Avoid

```csharp
try
{
    _cache.Remove(key);
}
catch { }
```

## Use

```csharp
try
{
    _cache.Remove(key);
}
catch (CacheUnavailableException ex)
{
    _logger.LogWarning(ex, "cannot evict {Key}, continuing cold", key);
}
```

Catch the narrowest exception type that can occur. Rethrow with a bare `throw;`
so the original stack trace survives; `throw ex;` restarts it at this line.

Never catch `OperationCanceledException` and continue: cancellation is a
decision the caller made, and it must reach them.
