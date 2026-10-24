# syntax=docker/dockerfile:1.7
FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock LICENSE ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo test --locked -j 4 && cargo build --release --locked -j 4 \
    && install -m 755 target/release/enginebox-stream-host /usr/local/bin/enginebox-stream-host

FROM debian:bookworm-slim AS godot
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
      amd64) godot_arch=x86_64; godot_sha=9aa00f7a605200940bce3027a567b782f49bd8e940dd06ae9e987bd65aee1b1467edd56ed84fcdcbdd44354bf613bdbb4e5d2913e925850368e150c59ed54c65 ;; \
      arm64) godot_arch=arm64; godot_sha=dd59918da086bd49bde2f5450b5e567ff8650cbde9abbd7b8f4ca1197ff8c609baa38834666d032deafb47099078d7822279e2a0e06e5665745468f26533e7e2 ;; \
      *) exit 1 ;; \
    esac && \
    curl -fsSL --retry 3 "https://github.com/godotengine/godot/releases/download/4.7.2-stable/Godot_v4.7.2-stable_linux.$godot_arch.zip" -o /tmp/godot.zip && \
    printf '%s  /tmp/godot.zip\n' "$godot_sha" | sha512sum -c - && \
    unzip -q /tmp/godot.zip -d /tmp/godot && \
    install -m 755 "/tmp/godot/Godot_v4.7.2-stable_linux.$godot_arch" /usr/local/bin/godot

FROM debian:bookworm-slim AS runtime
ARG SOURCE_REVISION
LABEL org.opencontainers.image.source="https://github.com/coop-collective/enginebox" \
      org.opencontainers.image.revision=$SOURCE_REVISION \
      org.opencontainers.image.title="Enginebox Godot stream host"
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates ffmpeg xvfb libgl1-mesa-dri libegl1 tini python3 \
    libx11-6 libxcursor1 libxinerama1 libgl1 libxi6 libxrandr2 libfontconfig1 libasound2 libudev1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 enginebox && useradd --uid 1000 --gid 1000 --no-create-home enginebox \
    && install -d -o 1000 -g 1000 -m 0700 /var/lib/enginebox-stream \
    && install -d -m 0755 /etc/enginebox /app /opt/enginebox/godot-check
COPY --from=build /usr/local/bin/enginebox-stream-host /usr/local/bin/enginebox-stream-host
COPY --from=godot /usr/local/bin/godot /usr/local/bin/godot
COPY LICENSE /usr/share/doc/enginebox-stream-host/LICENSE
COPY container/godot-host.py /usr/local/bin/enginebox-godot-host
COPY examples/godot-check /opt/enginebox/godot-check
USER 1000:1000
ENV HOME=/tmp PYTHONDONTWRITEBYTECODE=1
WORKDIR /app
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/bin/python3", "/usr/local/bin/enginebox-godot-host"]
