# Build only in release CI; Windows downloads the resulting credential-free .wsl.
FROM node:24.21.0-bookworm@sha256:3d27e5c11e5786e309ec3e03f93ae536eb36e6e5eb3714d5eb3300a36157add0 AS template
RUN apt-get update && apt-get install -y --no-install-recommends git fuse3 bindfs inotify-tools ca-certificates python3 ripgrep \
 && rm -rf /var/lib/apt/lists/* \
 && npm install --global pnpm@12.14.0 @openai/codex@0.163.0 @anthropic-ai/claude-code@2.1.302 \
 && mkdir -p /home/agent && chown 501:20 /home/agent
ENV HOME=/home/agent CODEX_HOME=/home/agent/.codex CI=1
RUN codex --version && claude --version
CMD ["bash"]

FROM rust:1.96-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml LICENSE NOTICE ./
COPY packages ./packages
RUN cargo build --release --locked -p enginebox-install --bin enginebox-node -p enginebox-cli --bin ebox

FROM node:24.21.0-bookworm@sha256:3d27e5c11e5786e309ec3e03f93ae536eb36e6e5eb3714d5eb3300a36157add0 AS layerfs-build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libfuse3-dev libssl-dev python3 curl git \
 && curl --fail --location --proto '=https' --proto-redir '=https' https://sh.rustup.rs -o /tmp/rustup.sh \
 && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.85.1
ENV PATH=/root/.cargo/bin:$PATH CARGO_BUILD_JOBS=3 LAYERFS_CONSTRUCTION_WORKERS=1
WORKDIR /layerfs
RUN git init && git remote add origin https://github.com/Ephemeral-AI-Lab/layerfs.git \
 && git fetch --depth=1 origin 968a5d0be0ea3e9456b06f9148b65523138e9490 \
 && git checkout --detach FETCH_HEAD && test "$(git rev-parse HEAD)" = 968a5d0be0ea3e9456b06f9148b65523138e9490 \
 && python3 -B core/tools/check_fuser_integrity.py \
 && cargo fetch --manifest-path core/Cargo.toml --locked \
 && cargo build --manifest-path core/Cargo.toml --offline --locked --release -p layerfs-daemon --bin layerfs-daemon \
 && npm --prefix sdk/js run build:native

FROM node:24.21.0-bookworm@sha256:3d27e5c11e5786e309ec3e03f93ae536eb36e6e5eb3714d5eb3300a36157add0 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends systemd systemd-sysv dbus iptables ca-certificates curl git fuse3 bindfs util-linux python3 jq openssl \
 && install -m 0755 -d /etc/apt/keyrings \
 && curl --fail --location --proto '=https' https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
 && echo 'deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable' > /etc/apt/sources.list.d/docker.list \
 && apt-get update && apt-get install -y --no-install-recommends docker-ce=5:29.3.0-1~debian.12~bookworm docker-ce-cli=5:29.3.0-1~debian.12~bookworm containerd.io \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl disable docker.service docker.socket containerd.service
COPY --from=rust-build /src/target/release/enginebox-node /src/target/release/ebox /usr/local/bin/
COPY --from=layerfs-build /layerfs/sdk/js /opt/enginebox/layerfs/sdk
COPY --from=layerfs-build /layerfs/core/target/release/layerfs-daemon /opt/enginebox/layerfs/daemon
COPY --from=layerfs-build /layerfs/LICENSE /opt/enginebox/layerfs/LICENSE
COPY --from=bundle /template.tar /opt/enginebox/template.tar
COPY packages/enginebox-install/wsl/runtime.sh /usr/local/sbin/enginebox-wsl
COPY packages/enginebox-install/wsl/qualify.py /opt/enginebox/qualify.py
COPY packages/enginebox-install/wsl/enginebox-docker.service packages/enginebox-install/wsl/enginebox-worker.service /etc/systemd/system/
COPY packages/enginebox-install/wsl/wsl.conf /etc/wsl.conf
COPY packages/enginebox-install/wsl/wsl-distribution.conf /etc/wsl-distribution.conf
RUN chmod 0755 /usr/local/sbin/enginebox-wsl \
 && mkdir -p /etc/enginebox /var/lib/enginebox /opt/enginebox/seed \
 && chmod 0700 /etc/enginebox /var/lib/enginebox \
 && touch /etc/enginebox/wsl-runtime \
 && printf 'Enginebox template workspace\n' > /opt/enginebox/seed/README.md \
 && systemctl enable enginebox-worker.service
CMD ["/sbin/init"]
