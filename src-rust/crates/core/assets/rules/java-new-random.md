---
description: Use ThreadLocalRandom for ordinary randomness and SecureRandom for anything that must be unguessable
condition: "new\\s+Random\\s*\\("
scope: "tool:Edit(*.{java,kt,kts}), tool:Write(*.{java,kt,kts})"
---

`java.util.Random` is a linear congruential generator with a 48-bit seed. Its
output is fully predictable from a handful of samples, so it must never produce
a token, a password, a session id or a nonce. Sharing one instance across
threads also contends on its atomic seed.

## Avoid

```java
private static final Random RANDOM = new Random();
String token = Long.toHexString(RANDOM.nextLong());
```

## Use

```java
// Ordinary randomness: sampling, jitter, shuffling.
int backoff = ThreadLocalRandom.current().nextInt(100, 500);

// Anything an attacker must not guess.
byte[] token = new byte[32];
SecureRandom.getInstanceStrong().nextBytes(token);
```

`ThreadLocalRandom` needs no shared instance and no lock. `SecureRandom` draws
from the operating system's entropy source.

Seed a `Random` explicitly only in a test that needs a reproducible sequence.
