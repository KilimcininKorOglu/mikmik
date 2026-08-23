---
description: Never call eval, system, exec, shell_exec or passthru on a value the program did not produce
condition:
  - "\\beval\\s*\\("
  - "\\b(system|shell_exec|passthru|proc_open)\\s*\\("
  - "\\bexec\\s*\\(\\s*[\"'][^\"']*[\"']\\s*\\."
scope: "tool:Edit(*.php), tool:Write(*.php)"
---

These functions hand a string to a shell or to the PHP parser. Any value that
reached the program from a request, a file or a database then decides what
runs. This is OWASP A03, injection.

## Avoid

```php
system("convert {$_POST['file']} out.png");
eval('$result = ' . $expression . ';');
```

## Use

Escape each argument, or better, avoid the shell:

```php
$cmd = ['convert', '--', $file, 'out.png'];
$proc = new Symfony\Component\Process\Process($cmd);
$proc->mustRun();
```

Without a process library, `escapeshellarg` on **every** interpolated value is
the minimum:

```php
system('convert ' . escapeshellarg($file) . ' out.png');
```

For `eval`, there is no safe form. Whatever the expression selects, express it
as a `match` or an array of allowed callables instead.
