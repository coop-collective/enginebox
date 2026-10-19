#!/bin/bash
# Follow-up to a completed campaign; never overwrites its first receipts.
set -euo pipefail
test -f /etc/enginebox-layerfs-validation
stamp="${1:?completed campaign identifier required}"
control="${2:?fresh control identifier required}"
case "$stamp" in *[!a-z0-9-]*|'') exit 2;; esac
case "$control" in *[!a-z0-9-]*|'') exit 2;; esac
run="/opt/layerfs-validation/$stamp"
test -f "$run/summary.json"
control_root="$run/$control"
test ! -e "$control_root"
install -d -m 755 "$control_root" "$control_root/seed"
image="$(docker image inspect enginebox-layerfs-validation:runtime --format '{{.Id}}')"
pnpm_volume="ebox-layerfs-$stamp-$control-pnpm"
cache_volume="ebox-layerfs-$stamp-$control-cache"
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
fresh_volume "$pnpm_volume"
fresh_volume "$cache_volume"
cp "$run/fixture/package.json" "$run/fixture/pnpm-lock.yaml" "$control_root/seed/"
docker run --rm -v "$control_root/seed:/seed" -v "$pnpm_volume:/pnpm-store" \
  -v "$cache_volume:/pnpm-cache" -w /seed "$image" bash -c \
  'set -euo pipefail; PNPM_CONFIG_CACHE_DIR=/pnpm-cache pnpm install --frozen-lockfile --ignore-scripts --store-dir /pnpm-store --reporter append-only; chmod -R a+rX /pnpm-store /pnpm-cache' > "$control_root/seed.log" 2>&1
for variant in ordinary frozen; do
  destination="$control_root/native-$variant"
  install -d -m 755 "$destination"
  cp "$run/fixture/package.json" "$run/fixture/pnpm-lock.yaml" "$destination/"
  chown -R 501:20 "$destination"
  args=()
  if [ "$variant" = frozen ]; then args+=(--frozen-store); fi
  set +e
  docker run --rm --network none --user 501:20 -e HOME=/tmp \
    -v "$destination:/native" -v "$pnpm_volume:/pnpm-store:ro" -v "$cache_volume:/seed-cache:ro" -w /native \
    "$image" bash -c 'set -euo pipefail; cp -R /seed-cache /tmp/pnpm-cache; chmod -R u+w /tmp/pnpm-cache; PNPM_CONFIG_CACHE_DIR=/tmp/pnpm-cache pnpm install --offline --frozen-lockfile --ignore-scripts --package-import-method=copy --store-dir /pnpm-store --reporter append-only "$@"; node -p '\''require("typescript/package.json").version'\''' bash "${args[@]}" > "$control_root/native-$variant.log" 2>&1
  status=$?
  set -e
  printf '%s\n' "$status" > "$control_root/native-$variant.exit"
done
test "$(cat "$control_root/native-ordinary.exit")" != 0
grep -q ERR_PNPM_NO_OFFLINE_TARBALL "$control_root/native-ordinary.log"
test "$(cat "$control_root/native-frozen.exit")" = 0
docker run --name "ebox-layerfs-$stamp-$control-client" --network host \
  -v /opt/layerfs-validation:/opt/layerfs-validation -v /var/run/docker.sock:/var/run/docker.sock \
  -v "ebox-layerfs-$stamp-store:/layerfs-store" \
  -e "EXPERIMENT_ROOT=$run" -e UPSTREAM_ROOT=/opt/layerfs-validation/upstream \
  -e "CONTROL_NAME=$control" -e "CACHE_VOLUME=$cache_volume" \
  -e "PNPM_VOLUME=$pnpm_volume" -e "LAYERFS_HOME=$run/home" -e "LAYERFS_IMAGE=$image" \
  -e "LAYERFS_VOLUME=ebox-layerfs-$stamp-store" -e "LAYERFS_CONTAINER_PREFIX=ebox-layerfs-$stamp-" \
  -e LAYERFS_DAEMON=/opt/layerfs-validation/upstream/core/target/release/layerfs-daemon \
  -e LAYERFS_NATIVE=/opt/layerfs-validation/upstream/sdk/js/native/layerfs.node \
  -e LAYERFS_CONSTRUCTION_WORKERS=1 -e LAYERFS_STORE_PATH=/layerfs-store/global/store.sqlite \
  "$image" timeout 180 node /opt/layerfs-validation/harness/dependency-control.mjs > "$control_root/fuse.log" 2>&1
