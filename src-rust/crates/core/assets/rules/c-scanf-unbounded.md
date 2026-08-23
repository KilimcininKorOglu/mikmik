---
description: A bare %s in scanf has no bound; give every string conversion a width
condition: "\\b(scanf|fscanf|sscanf)\\s*\\(\\s*[^;]*\"[^\"]*%s"
scope: "tool:Edit(*.{c,h,cc,cpp,hpp,cxx}), tool:Write(*.{c,h,cc,cpp,hpp,cxx})"
---

`%s` in a `scanf` format reads until whitespace, however long that is. The
destination buffer's size never reaches the function, so the write runs past
its end exactly like `strcpy`.

## Avoid

```c
char name[32];
scanf("%s", name);
sscanf(line, "%s %s", user, host);
```

## Use

Give the conversion a maximum field width, one less than the buffer, to leave
room for the terminating byte:

```c
char name[32];
if (scanf("%31s", name) != 1) {
    return -1;
}
```

The width must be a literal in the format string, so it cannot be written as
`sizeof name`. Define the buffer size and the width together:

```c
#define NAME_MAX_LEN 31
char name[NAME_MAX_LEN + 1];
scanf("%" STR(NAME_MAX_LEN) "s", name);
```

Always check the return value: it is the number of conversions that succeeded,
not the number you asked for.

For a whole line, `fgets` with `sizeof buf` is simpler and already bounded.
