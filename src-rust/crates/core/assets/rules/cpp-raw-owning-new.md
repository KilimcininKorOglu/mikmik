---
description: Do not manage memory with bare new and delete; use make_unique, make_shared or a container
condition:
  - "=\\s*new\\s+[A-Za-z_][\\w:<>]*\\s*[({\\[]"
  - "\\bdelete\\s+(\\[\\s*\\]\\s*)?[a-zA-Z_]\\w*\\s*;"
scope: "tool:Edit(*.{cc,cpp,hpp,cxx,hh}), tool:Write(*.{cc,cpp,hpp,cxx,hh})"
---

A raw `new` puts the matching `delete` somewhere else, and every path between
the two has to reach it: every early return, every `break`, every thrown
exception. One that does not leaks, and one that runs twice corrupts the heap.

## Avoid

```cpp
Widget *w = new Widget(config);
if (!w->valid()) {
    return nullptr;          // leaked
}
use(*w);
delete w;
```

## Use

```cpp
auto w = std::make_unique<Widget>(config);
if (!w->valid()) {
    return nullptr;          // freed on the way out
}
use(*w);
```

| Want | Use |
|---|---|
| One owner | `std::unique_ptr<T>`, built with `std::make_unique` |
| Shared ownership | `std::shared_ptr<T>`, built with `std::make_shared` |
| An array | `std::vector<T>` |
| A non-owning reference | `T&`, `T*` or `std::span<T>`, and never delete it |

`make_unique` and `make_shared` also close the gap where an exception between
the allocation and the constructor would leak.

Bare `new` remains correct inside a container or a handle class that exists to
own the memory, and for placement new. Say so in a comment when you write one.
