FROM node:24-bookworm-slim@sha256:a5f4366e834695d5f363b920c104953a535d3a9864d1bdf74566a0ebe01daee5
RUN apt-get update && apt-get install --yes --no-install-recommends ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/*
RUN npm install --global pnpm@12.15.0
WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY packages/fleet-protocol/package.json packages/fleet-protocol/package.json
COPY packages/fleet-controller/package.json packages/fleet-controller/package.json
COPY packages/fleet-node/package.json packages/fleet-node/package.json
COPY packages/fleet-dev/package.json packages/fleet-dev/package.json
RUN pnpm install --frozen-lockfile --filter '@enginebox/fleet-controller...' --filter '@enginebox/fleet-node...'
COPY packages/fleet-protocol packages/fleet-protocol
COPY packages/fleet-controller packages/fleet-controller
COPY packages/fleet-node packages/fleet-node
ENV CLOUDFLARE_SEND_METRICS=false
