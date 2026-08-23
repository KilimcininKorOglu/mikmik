---
description: Never run eval, exec or pickle.loads on data the program did not produce itself
condition:
  - "\\beval\\s*\\("
  - "\\bexec\\s*\\("
  - "pickle\\.loads?\\s*\\("
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

`eval` and `exec` run whatever they are given, with the caller's privileges.
`pickle` is worse: unpickling calls `__reduce__` on the payload, so loading a
pickle is running its author's code. This is OWASP A08, deserialization.

## Avoid

```python
config = eval(open("config.txt").read())
state = pickle.loads(response.content)
```

## Use

| Want | Reach for |
|---|---|
| A literal from text | `ast.literal_eval` |
| Structured data | `json.loads`, or a schema library |
| A value chosen by name | a `dict` of allowed callables |
| Data between your own processes | `json`, or `pickle` over a channel only you can write |

```python
import ast
config = ast.literal_eval(text)   # literals only, no calls, no imports
```

`ast.literal_eval` accepts strings, numbers, tuples, lists, dicts, sets,
booleans and `None`, and nothing else.
