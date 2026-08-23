---
description: SimpleDateFormat is not thread safe; use DateTimeFormatter from java.time
condition: "\\bSimpleDateFormat\\s*\\("
scope: "tool:Edit(*.{java,kt,kts}), tool:Write(*.{java,kt,kts})"
---

`SimpleDateFormat` keeps parsing state in a field. Two threads sharing one
instance corrupt each other's work, and the result is a wrong date rather than
an exception, so the bug reaches production data. A `static final` formatter is
the usual way this happens.

## Avoid

```java
private static final SimpleDateFormat FMT =
    new SimpleDateFormat("yyyy-MM-dd");

String today = FMT.format(new Date());
```

## Use

```java
private static final DateTimeFormatter FMT =
    DateTimeFormatter.ofPattern("yyyy-MM-dd");

String today = LocalDate.now().format(FMT);
```

`DateTimeFormatter` is immutable and safe to share. The whole `java.time` API
is: `Instant`, `LocalDate`, `ZonedDateTime` and `Duration` replace `Date`,
`Calendar` and `TimeZone`.

Always name the zone explicitly when the value crosses a boundary; the default
zone is the machine's, and the machine changes.
