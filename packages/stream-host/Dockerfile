# syntax=docker/dockerfile:1.7
FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock LICENSE ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -j 4 && cp target/release/enginebox-stream-host /usr/local/bin/enginebox-stream-host

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates ffmpeg xvfb libgl1-mesa-dri libegl1 tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 enginebox && useradd --uid 1000 --gid 1000 --no-create-home enginebox \
    && install -d -o 1000 -g 1000 -m 0700 /var/lib/enginebox-stream \
    && install -d -m 0755 /etc/enginebox /app
COPY --from=build /usr/local/bin/enginebox-stream-host /usr/local/bin/enginebox-stream-host
USER 1000:1000
ENV HOME=/tmp
WORKDIR /app
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/enginebox-stream-host"]

# Test-only native X11 application, never included in the reusable runtime image.
FROM debian:bookworm-slim AS fixture-build
RUN apt-get update && apt-get install --no-install-recommends -y gcc libc6-dev libx11-dev \
    && rm -rf /var/lib/apt/lists/*
COPY test/window.c /window.c
RUN gcc -O2 -Wall -Wextra /window.c -lX11 -o /window
FROM build AS test-build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo test --release --locked --no-run --message-format=json -j 4 > /tmp/test-artifacts.json \
    && test_binary=$(sed -n '/"test":true/s/.*"executable":"\([^"]*\)".*/\1/p' /tmp/test-artifacts.json | tail -n 1) \
    && test -n "$test_binary" && cp "$test_binary" /usr/local/bin/stream-host-tests
FROM runtime AS test-runtime
COPY --from=fixture-build /window /usr/local/bin/enginebox-test-window
COPY --from=test-build /usr/local/bin/stream-host-tests /usr/local/bin/stream-host-tests

# An unqualified `docker build` must never include the test application.
FROM runtime AS default
