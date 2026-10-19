#!/bin/bash
set -euo pipefail
test -f /etc/enginebox-layerfs-validation
cd /opt/layerfs-validation
stamp="${1:?fresh run identifier required}"
case "$stamp" in *[!a-z0-9-]*|'') exit 2;; esac
run="/opt/layerfs-validation/$stamp"
test ! -e "$run"
install -d -m 700 "$run" "$run/home"
diagnostics() {
  status=$?
  trap - EXIT
  set +e
  docker ps -a --no-trunc > "$run/containers-after.txt"
  docker version > "$run/docker-version.txt" 2>&1
  findmnt -T "$run" -J > "$run/guest-filesystem.json" 2>&1
  for container in $(docker ps -a --format '{{.Names}}' | sed -n "/^ebox-layerfs-$stamp-/p"); do
    docker inspect "$container" > "$run/inspect-$container.json" 2>&1
    docker logs --tail 2000 "$container" > "$run/log-$container.txt" 2>&1
  done
  printf '%s\n' "$status" > "$run/exit-status.txt"
  exit "$status"
}
trap diagnostics EXIT
image="$(docker image inspect enginebox-layerfs-validation:runtime --format '{{.Id}}')"
volume="ebox-layerfs-$stamp-store"
pnpm_volume="ebox-layerfs-$stamp-pnpm"
volume_owner="$(cat /proc/sys/kernel/random/uuid)"
fresh_volume() {
  if docker volume inspect "$1" >/dev/null 2>&1; then
    echo "refusing existing Docker volume: $1" >&2
    exit 1
  fi
  docker volume create --label enginebox.experiment="$stamp" --label enginebox.validation-owner="$volume_owner" "$1"
  test "$(docker volume inspect "$1" --format '{{index .Labels "enginebox.validation-owner"}}')" = "$volume_owner"
  test "$(docker volume inspect "$1" --format '{{index .Labels "enginebox.experiment"}}')" = "$stamp"
}
fresh_volume "$volume"
fresh_volume "$pnpm_volume"
docker run --rm --name "ebox-layerfs-$stamp-fixture" \
  -v /opt/layerfs-validation:/opt/layerfs-validation \
  -v "$pnpm_volume:/pnpm-store" "$image" bash -c '
set -euo pipefail
run="$1"
node /opt/layerfs-validation/harness/fixture.mjs "$run/fixture"
cd "$run/fixture"
pnpm install --store-dir /pnpm-store --ignore-scripts --reporter append-only
node node_modules/typescript/bin/tsc -b --verbose
git add .
git commit -qm seed
echo "shared read-only store marker" > /pnpm-store/marker.txt
chmod -R a+rX /pnpm-store
node validation-probe.mjs metadata
node validation-manifest.mjs . "$run/fixture-manifest.json"
' bash "$run" > "$run/fixture.log" 2>&1
docker run --name "ebox-layerfs-$stamp-client" --network host \
  -v /opt/layerfs-validation:/opt/layerfs-validation \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$volume:/layerfs-store" \
  -e "EXPERIMENT_ROOT=$run" -e "UPSTREAM_ROOT=/opt/layerfs-validation/upstream" \
  -e "PNPM_VOLUME=$pnpm_volume" -e "LAYERFS_HOME=$run/home" \
  -e "LAYERFS_IMAGE=$image" -e "LAYERFS_VOLUME=$volume" \
  -e "LAYERFS_CONTAINER_PREFIX=ebox-layerfs-$stamp-" \
  -e LAYERFS_DAEMON=/opt/layerfs-validation/upstream/core/target/release/layerfs-daemon \
  -e LAYERFS_NATIVE=/opt/layerfs-validation/upstream/sdk/js/native/layerfs.node \
  -e LAYERFS_CONSTRUCTION_WORKERS=1 \
  -e LAYERFS_STORE_PATH=/layerfs-store/global/store.sqlite \
  "$image" timeout 600 node /opt/layerfs-validation/harness/experiment.mjs > "$run/run.log" 2>&1
