---
description: Use yaml.safe_load; yaml.load can construct arbitrary Python objects
condition: "yaml\\.load\\s*\\("
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

PyYAML's `load` understands tags such as `!!python/object/apply`, which call
Python during parsing. A YAML file is therefore executable, and loading one you
did not write runs its author's code. This is OWASP A08, deserialization.

## Avoid

```python
config = yaml.load(open("config.yml"))
```

## Use

```python
config = yaml.safe_load(open("config.yml"))
```

`safe_load` builds only the standard YAML types. If you genuinely need a custom
tag, register the constructor on `SafeLoader` rather than opening the full
loader.
