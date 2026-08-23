---
description: Never unserialize data from outside the program; use json_decode
condition: "\\bunserialize\\s*\\("
scope: "tool:Edit(*.php), tool:Write(*.php)"
---

`unserialize` rebuilds objects, and rebuilding an object runs its `__wakeup`
and `__destruct` methods. An attacker who controls the string picks which
classes are constructed and with what properties, then chains them into a call
they want. This is OWASP A08, deserialization.

## Avoid

```php
$session = unserialize($_COOKIE['state']);
$cached  = unserialize($redis->get($key));
```

A cookie is attacker-controlled by definition. A cache is attacker-controlled as
soon as anything else can write to it.

## Use

```php
$session = json_decode($_COOKIE['state'], true, 512, JSON_THROW_ON_ERROR);
```

JSON builds arrays and scalars only, so no class is constructed and no method
runs.

If the format cannot change, `unserialize($data, ['allowed_classes' => false])`
refuses every object. Sign the payload as well, so only your own writer is
accepted.
