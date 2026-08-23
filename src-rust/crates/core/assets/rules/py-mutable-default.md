---
description: Never use a list, dict or set as a default argument; the same object is shared by every call
condition: "def\\s+\\w+\\s*\\([^)]*=\\s*(\\[\\]|\\{\\}|set\\(\\)|dict\\(\\)|list\\(\\))"
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

Python evaluates a default argument once, when the `def` runs. Every call that
does not pass the argument gets the **same** object, so one call's changes are
visible to the next.

## Avoid

```python
def add_tag(name, tags=[]):
    tags.append(name)
    return tags

add_tag("a")   # ['a']
add_tag("b")   # ['a', 'b'] — the list survived
```

## Use

```python
def add_tag(name, tags=None):
    tags = [] if tags is None else tags
    tags.append(name)
    return tags
```

`None` is the sentinel because it is immutable and cannot be a caller's list.
The same applies to `{}`, `set()` and any other object built at definition time.
