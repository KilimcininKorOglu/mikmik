---
description: Never leave a catch block empty; handle the exception, rethrow it, or say in the code why it is ignorable
condition: "catch\\s*\\([^)]*\\)\\s*\\{\\s*\\}"
scope: "tool:Edit(*.{java,kt,kts}), tool:Write(*.{java,kt,kts})"
---

An empty catch block turns a failure into silence. The program carries on with
a value it never got, and the real symptom appears somewhere else entirely.

## Avoid

```java
try {
    cache.evict(key);
} catch (CacheException e) {
}
```

## Use

Pick one, and make the choice visible:

```java
// Act on it.
catch (CacheException e) {
    log.warn("cannot evict {}, continuing with a cold cache", key, e);
}

// Or let the caller decide.
catch (CacheException e) {
    throw new OrderFailed("cache unavailable", e);
}
```

When the exception genuinely cannot matter, name the variable `ignored` and say
why in a comment. A reader then knows it was a decision, not an oversight.

Never swallow `InterruptedException`: restore the flag with
`Thread.currentThread().interrupt()` before returning.
