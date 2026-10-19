#!/bin/bash
set -euo pipefail
test -f /etc/enginebox-layerfs-validation
cd /opt/layerfs-validation
docker build --tag enginebox-layerfs-validation:runtime --file Dockerfile .
docker image inspect enginebox-layerfs-validation:runtime > runtime-image.json
docker run --name ebox-layerfs-builder --cpus 4 --memory 6g \
  -v /opt/layerfs-validation:/opt/layerfs-validation \
  -w /opt/layerfs-validation/upstream enginebox-layerfs-validation:runtime bash -lc '
set -euo pipefail
apt-get update
apt-get install -y --no-install-recommends build-essential pkg-config libfuse3-dev libssl-dev python3 curl git
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.85.1
export PATH=/root/.cargo/bin:$PATH
export CARGO_BUILD_JOBS=4
export LAYERFS_CONSTRUCTION_WORKERS=1
# The clean source archive has no Git database; the provenance checker uses
# git ls-files --others, so create only an empty index context, not a fake pin.
git init --quiet
python3 -B core/tools/check_fuser_integrity.py
cargo +1.85.1 fetch --manifest-path core/Cargo.toml --locked
cargo +1.85.1 build --manifest-path core/Cargo.toml --offline --locked --release -p layerfs-daemon --bin layerfs-daemon
npm --prefix sdk/js run build:native
sha256sum core/target/release/layerfs-daemon sdk/js/native/layerfs.node
rustc --version
node --version
pnpm --version
'
