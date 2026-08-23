---
description: Never write a credential into a tracked file
condition:
  - "(?i)(api[_-]?key|secret|password|passwd|token)\\s*[:=]\\s*[\"'][A-Za-z0-9_\\-/+]{16,}[\"']"
  - "-----BEGIN (RSA |EC |OPENSSH |PGP )?PRIVATE KEY-----"
  - "(?i)aws_secret_access_key\\s*="
  - "sk-[A-Za-z0-9]{20,}"
  - "gh[pousr]_[A-Za-z0-9]{30,}"
---

What looks like a credential is about to be written into a file. A secret in a
repository is compromised the moment it is pushed, and rotating it is the only
fix: deleting the line leaves it in the history.

## Instead

|Need|Use|
|---|---|
|A secret the program reads|An environment variable, read at startup|
|A secret a developer sets|`.env`, listed in `.gitignore`|
|A secret in CI|The CI provider's secret store|
|An example in documentation|An obvious placeholder: `sk-REPLACE_ME`|

```rust
// Bad.
const API_KEY: &str = "sk-abc123...";

// Good.
let api_key = std::env::var("SERVICE_API_KEY")
    .map_err(|_| anyhow::anyhow!("SERVICE_API_KEY is not set"))?;
```

If this is a placeholder, a test fixture or an example, make that obvious in
the value itself and carry on.

If it is a real credential that is already committed somewhere, say so plainly
and treat it as compromised.
