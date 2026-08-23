---
description: Use builtin generics (list[str], dict[str, int]) rather than typing.List and typing.Dict
condition:
  - "from\\s+typing\\s+import\\s+[^\\n]*\\b(List|Dict|Set|Tuple|FrozenSet|Type)\\b"
  - "typing\\.(List|Dict|Set|Tuple|FrozenSet|Type)\\["
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

PEP 585 made the builtin containers subscriptable in Python 3.9. The `typing`
aliases have been deprecated since then, and they read as a second vocabulary
for types the language already names.

## Avoid

```python
from typing import Dict, List, Optional, Tuple

def group(rows: List[str]) -> Dict[str, List[str]]: ...
def head(xs: List[int]) -> Optional[int]: ...
```

## Use

```python
def group(rows: list[str]) -> dict[str, list[str]]: ...
def head(xs: list[int]) -> int | None: ...
```

`X | None` replaces `Optional[X]` and `X | Y` replaces `Union[X, Y]` from
Python 3.10. `typing` is still where `Protocol`, `TypeVar`, `Callable`,
`Literal` and `Any` live.

Keep the old spelling only when the module must run on Python 3.8.
