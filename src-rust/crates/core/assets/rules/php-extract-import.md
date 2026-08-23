---
description: Never call extract on request data; it lets a request name your local variables
condition:
  - "\\bextract\\s*\\("
  - "\\bimport_request_variables\\s*\\("
scope: "tool:Edit(*.php), tool:Write(*.php)"
---

`extract` turns each array key into a local variable. Given request data, the
request chooses which of your variables to overwrite, including one you already
set to a checked value.

## Avoid

```php
$is_admin = check_admin($user);
extract($_POST);          // a POST field named is_admin now wins
if ($is_admin) { ... }
```

## Use

Read the keys you expect, by name:

```php
$name  = $_POST['name']  ?? '';
$email = $_POST['email'] ?? '';
```

Or validate the whole shape once with a validator, and read from the result.

If `extract` is unavoidable on data you control, pass `EXTR_SKIP` so it can
never overwrite an existing variable. On request data, no flag makes it safe.
