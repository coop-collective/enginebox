# Build from repository root; Playwright and its browser image have matching versions.
FROM mcr.microsoft.com/playwright:v1.64.1-noble
WORKDIR /workspace
RUN npm install --global pnpm@12.14.0
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY packages/stream-host/package.json ./packages/stream-host/package.json
# The test image does not vendor or compile the host Rust workspace.
RUN sed -i 's/enabled: true/enabled: false/' pnpm-workspace.yaml \
    && pnpm install --frozen-lockfile --filter @enginebox/stream-host --ignore-scripts
ENV PLAYWRIGHT_PACKAGE=/workspace/packages/stream-host/package.json
WORKDIR /test
ENTRYPOINT ["node"]
