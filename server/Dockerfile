# Multi-stage: the toolchain never ships in the runtime image.
FROM rust:1.85-bookworm AS build

WORKDIR /build

# Dependencies first, so a source-only change reuses the cached layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Touch so cargo does not reuse the stub's timestamp and skip the real build.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN useradd --system --create-home --uid 10002 mikmik
USER mikmik
WORKDIR /home/mikmik

COPY --from=build /build/target/release/mikmik-server /usr/local/bin/mikmik-server

# MIKMIK_SERVER_SECRET has no default on purpose: the server refuses to start
# without one, and a default would be a published encryption key.
ENV MIKMIK_SERVER_BIND=0.0.0.0:8420
ENV MIKMIK_SERVER_DB=/data/mikmik-server.sqlite

EXPOSE 8420

# The database lives outside the image, so an upgrade does not take the
# accounts with it.
VOLUME ["/data"]

# No shell form, so the process is PID 1 and receives SIGTERM directly for the
# graceful shutdown path.
ENTRYPOINT ["/usr/local/bin/mikmik-server"]
