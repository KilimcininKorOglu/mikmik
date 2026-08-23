---
description: "Do not assert an inline object type and immediately read a property; (x as { y: T }).y trusts an unchecked shape"
condition: "\\(\\s*[^()]*\\bas\\s+\\{[^{}]*\\}\\s*\\)\\s*(\\??\\.|\\[)"
scope: "tool:Edit(*.{ts,tsx,mts,cts}), tool:Write(*.{ts,tsx,mts,cts})"
---

## Do not inline-cast an object type for member access

`(value as { content: unknown }).content` invents an unchecked shape, then trusts it for the read. If `value` does not have that shape, the read is silently wrong and no type error fires.

## Why

- An unchecked assertion suppresses the error. It proves nothing about the shape.
- It hides the lie locally: a reader cannot tell whether `value` was ever validated.
- The right answer is usually runtime narrowing, or a validated type at the boundary.

## Avoid

```ts
const content = (value as { content: unknown }).content;
const id = (resp as { data: { id: string } }).data.id;
const name = (payload as { name?: string })?.name;
const flag = (opts as { enabled: boolean })["enabled"];
```

## Use

At a boundary, parse with a schema when a validator is available: validate once, then read a fully typed value.

```ts
import { z } from "zod";

const Resp = z.object({ data: z.object({ id: z.string() }) });

const resp = Resp.parse(raw); // throws on bad input; resp.data.id is a typed string
const id = resp.data.id;
```

For a one-off field read, narrow with `in` or `typeof`; the access is then checked. After `"content" in value`, TypeScript infers the property as `unknown`:

```ts
if (value && typeof value === "object" && "content" in value) {
	const content = value.content; // unknown, so validate before use
}
```

## Choose: guard, schema, or unchecked cast

- Data from outside (network or RPC, parsed JSON, config files, environment variables, CLI or IPC, persisted blobs), or a shape reused across the codebase: **schema parse**. Runtime validation, typed output, and a clear error on a bad shape.
- An in-process value the compiler lost track of (a generic `unknown`, a union to discriminate, a one-off read of one or two fields): **type guard** with `in` or `typeof`. No dependency, and it checks exactly what you write, so keep its surface small.
- You know more than the compiler **and** a runtime check is impossible or meaningless (a well-known DOM node, two structurally identical types inference cannot unify, a wrong or inexpressible library type, `as const`): **unchecked cast**. Assign it to a named const with a one-line reason. Never on raw external input, and never on an inline member access.
