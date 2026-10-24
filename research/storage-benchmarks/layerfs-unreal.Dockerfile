# Disposable research toolchain. Game assets are mounted only at runtime.
FROM node:24-bookworm-slim@sha256:a5f4366e834695d5f363b920c104953a535d3a9864d1bdf74566a0ebe01daee5 AS node
FROM rust:1.85.1-bookworm@sha256:e51d0265072d2d9d5d320f6a44dde6b9ef13653b035098febd68cce8fa7c0bc4
COPY --from=node /usr/local/bin/node /usr/local/bin/node
RUN apt-get update && apt-get install -y --no-install-recommends \
    fuse3 libfuse3-dev python3 sqlite3 pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_BUILD_JOBS=2 LAYERFS_CONSTRUCTION_WORKERS=1
CMD ["bash"]
