---
description: Catch the exception you expect; never write a bare except, and never swallow one with pass
condition:
  - "except\\s*:"
  - "except\\s+(Base)?Exception\\s*:\\s*\\n\\s*pass\\b"
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

A bare `except:` catches `KeyboardInterrupt` and `SystemExit` as well, so it
takes away the user's ability to stop the program. `except Exception: pass`
keeps running with a broken state and reports nothing, so the failure surfaces
later somewhere unrelated.

## Avoid

```python
try:
    value = parse(raw)
except:
    value = None

try:
    cleanup()
except Exception:
    pass
```

## Use

```python
try:
    value = parse(raw)
except (ValueError, KeyError) as e:
    raise ConfigError(f"cannot parse {raw!r}") from e
```

When a failure really is expected and ignorable, say so in the code:

```python
except FileNotFoundError:
    logger.debug("no cache file yet")
```

Catch the narrowest exception that can happen. Re-raise with `from e` so the
original traceback survives.
