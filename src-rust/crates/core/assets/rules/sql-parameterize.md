---
description: Build every SQL query with parameters, never by joining strings
condition:
  - "(?i)(SELECT|INSERT|UPDATE|DELETE)\\s[^\"'\\n]{0,120}(WHERE|VALUES|SET)[^\"'\\n]{0,60}[\"']\\s*[+.]\\s*"
  - "(?i)f[\"'](SELECT|INSERT|UPDATE|DELETE)[^\"']*\\{"
  - "(?i)format!\\(\\s*\"(SELECT|INSERT|UPDATE|DELETE)"
  - "(?i)\\.(query|execute|exec|raw)\\([^)]*\\$\\{"
---

A query built by joining strings runs whatever the value contains. That is SQL
injection, and it is the oldest hole in the list.

## Use instead

Every driver takes parameters. Pass the value, never paste it.

```rust
// Bad.
let sql = format!("SELECT * FROM users WHERE email = '{email}'");

// Good.
sqlx::query("SELECT * FROM users WHERE email = ?").bind(email)
```

```python
# Bad.
cur.execute(f"DELETE FROM sessions WHERE id = {session_id}")

# Good.
cur.execute("DELETE FROM sessions WHERE id = %s", (session_id,))
```

```js
// Bad.
db.query(`UPDATE plans SET name = '${name}' WHERE id = ${id}`);

// Good.
db.query("UPDATE plans SET name = $1 WHERE id = $2", [name, id]);
```

## What cannot be a parameter

A table or column name is not a value and no driver will bind it. Pick it from
a fixed list you wrote:

```rust
let column = match sort_by {
    "created" => "created_at",
    "name" => "name",
    _ => return Err(anyhow::anyhow!("unknown sort column")),
};
```

Never interpolate an identifier straight from a request.
