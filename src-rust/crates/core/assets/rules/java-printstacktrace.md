---
description: Never call printStackTrace; log the exception through the logger the application already uses
condition: "\\.printStackTrace\\s*\\("
scope: "tool:Edit(*.{java,kt,kts}), tool:Write(*.{java,kt,kts})"
---

`printStackTrace()` writes to `System.err`, which no log aggregator reads, no
log level filters, and no correlation id reaches. In a container it is often
discarded outright, so the failure leaves no record anywhere.

## Avoid

```java
try {
    process(order);
} catch (IOException e) {
    e.printStackTrace();
}
```

## Use

```java
private static final Logger log = LoggerFactory.getLogger(OrderService.class);

try {
    process(order);
} catch (IOException e) {
    log.error("cannot process order {}", order.id(), e);
}
```

Pass the exception as the last argument. SLF4J takes it as the throwable rather
than as a format parameter, so the stack trace is logged in full.

If the caller can act on the failure, throw instead of logging.
