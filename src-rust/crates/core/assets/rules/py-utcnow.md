---
description: datetime.utcnow returns a naive datetime and is deprecated; use datetime.now(timezone.utc)
condition:
  - "datetime\\.utcnow\\s*\\("
  - "\\.utcfromtimestamp\\s*\\("
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

`datetime.utcnow()` returns a datetime with **no** timezone attached, even
though the value is UTC. Comparing it with an aware datetime raises
`TypeError`, and formatting it produces a timestamp that claims local time.
Python 3.12 deprecates both this and `utcfromtimestamp`.

## Avoid

```python
from datetime import datetime

now = datetime.utcnow()                 # naive, but holds UTC
stamp = datetime.utcfromtimestamp(ts)   # same problem
```

## Use

```python
from datetime import datetime, timezone

now = datetime.now(timezone.utc)
stamp = datetime.fromtimestamp(ts, timezone.utc)
```

On Python 3.11 and later, `datetime.UTC` is a shorter spelling of
`timezone.utc`.
