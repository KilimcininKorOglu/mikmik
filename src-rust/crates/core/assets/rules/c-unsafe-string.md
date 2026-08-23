---
description: strcpy, strcat, sprintf and gets write past the end of a buffer; use the bounded forms
condition: "\\b(strcpy|strcat|sprintf|vsprintf|gets)\\s*\\("
scope: "tool:Edit(*.{c,h,cc,cpp,hpp,cxx}), tool:Write(*.{c,h,cc,cpp,hpp,cxx})"
---

None of these functions knows how large the destination is. They copy until the
source ends, so a source longer than the buffer overwrites whatever follows it:
other locals, the saved frame pointer, the return address. This is the classic
stack overflow, and it is still the most common memory-safety defect in C.
`gets` cannot be used safely at all and was removed in C11.

## Avoid

```c
char path[256];
strcpy(path, argv[1]);
strcat(path, "/config");
sprintf(path, "%s/%s", dir, name);
```

## Use

```c
char path[256];
if (snprintf(path, sizeof path, "%s/%s", dir, name) >= (int)sizeof path) {
    return -1;              /* truncated: treat it as a failure */
}
```

| Instead of | Use | Check |
|---|---|---|
| `strcpy` | `snprintf(dst, sizeof dst, "%s", src)` | return value against the size |
| `strcat` | `snprintf` with both parts | same |
| `sprintf` | `snprintf` | same |
| `gets` | `fgets(buf, sizeof buf, stdin)` | `NULL`, and strip the newline |

Always test `snprintf`'s return value. It reports the length it **wanted**, so a
value at or above the buffer size means the result was truncated.

In C++, use `std::string` and `std::format`, and the question does not arise.
