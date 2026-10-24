FROM node:24-bookworm-slim@sha256:a5f4366e834695d5f363b920c104953a535d3a9864d1bdf74566a0ebe01daee5
RUN apt-get update && apt-get install -y --no-install-recommends git python3 util-linux ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global --ignore-scripts --no-audit --no-fund pnpm@12.15.0
