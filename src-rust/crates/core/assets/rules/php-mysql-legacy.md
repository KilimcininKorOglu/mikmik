---
description: The mysql_* functions were removed in PHP 7; use PDO or mysqli with prepared statements
condition: "\\bmysql_(query|connect|fetch_(array|assoc|row)|real_escape_string|select_db)\\s*\\("
scope: "tool:Edit(*.php), tool:Write(*.php)"
---

The `mysql_*` extension was deprecated in PHP 5.5 and removed in PHP 7.0, so
this code does not run at all on any supported version. It also had no
placeholder support, so every call site built SQL by concatenation.

## Avoid

```php
$result = mysql_query("SELECT * FROM users WHERE id = " . $_GET['id']);
```

## Use

```php
$stmt = $pdo->prepare('SELECT * FROM users WHERE id = ?');
$stmt->execute([$_GET['id']]);
$user = $stmt->fetch(PDO::FETCH_ASSOC);
```

Construct the connection with the error mode on, so a failed query throws
rather than returning `false`:

```php
$pdo = new PDO($dsn, $user, $pass, [
    PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
    PDO::ATTR_EMULATE_PREPARES => false,
]);
```

`ATTR_EMULATE_PREPARES => false` makes the driver send the statement and the
values separately, which is what makes a placeholder a real boundary.
