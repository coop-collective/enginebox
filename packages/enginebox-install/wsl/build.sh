#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
bundle_dir="$(mktemp -d)"
trap 'rm -rf "$bundle_dir"' EXIT
output="${1:?Pass an output directory}"
mkdir -p "$output"
recipe=packages/enginebox-install/wsl/Dockerfile
build_id="$(git rev-parse --short HEAD)"
docker build --platform linux/amd64 --target template -f "$recipe" -t "enginebox-wsl-template:$build_id" .
docker save "enginebox-wsl-template:$build_id" -o "$bundle_dir/template.tar"
docker build --platform linux/amd64 --build-context "bundle=$bundle_dir" --target runtime -f "$recipe" -t "enginebox-wsl-runtime:$build_id" .
container_id="$(docker create "enginebox-wsl-runtime:$build_id")"
trap 'docker rm "$container_id" >/dev/null; rm -rf "$bundle_dir"' EXIT
docker export "$container_id" | gzip -1 > "$output/enginebox.wsl"
sha256sum "$output/enginebox.wsl" > "$output/SHA256SUMS"
