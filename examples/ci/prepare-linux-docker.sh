#!/bin/sh
# Run as root only inside a fresh, credential-free Ubuntu 24.04 CI template clone.
set -eu
export DEBIAN_FRONTEND=noninteractive
test "$(id -u)" = 0
. /etc/os-release
test "$ID" = ubuntu && test "$VERSION_ID" = 24.04
runner_user=${ENGINEBOX_CI_USER:-agent}
runner_home=$(getent passwd "$runner_user" | cut -d: -f6)
test -n "$runner_home" && test "$(id -u "$runner_user")" != 0
test -x /opt/actions-runner/bin/Runner.Listener
for file in .runner .credentials .credentials_rsaparams; do
  test ! -e "/opt/actions-runner/$file"
done
test ! -e "$runner_home/ci-runner"
enginebox-host clean-base-check --home "$runner_home"

# Versions qualified on 2026-11-07. Fail if these packages are unavailable;
# changing a pin requires a new disposable-template qualification.
apt-get update -qq
apt-get install -y --no-install-recommends \
  docker.io=29.1.3-0ubuntu3~24.04.2 \
  docker-compose-v2=2.40.3+ds1-0ubuntu1~24.04.1 \
  docker-buildx=0.30.1-0ubuntu1~24.04.1 \
  git-lfs=3.4.1-1ubuntu0.4
usermod -aG docker "$runner_user"
systemctl enable --now docker
su - "$runner_user" -c 'test "$(id -u)" != 0; docker info --format "Docker {{.ServerVersion}} {{.Architecture}} {{.OSType}}"; docker compose version; docker buildx version; git lfs version; sudo -n true'
enginebox-host clean-base-check --home "$runner_home"
apt-get clean
dpkg-query -W -f='${Package} ${Version}\n' docker.io docker-compose-v2 docker-buildx git-lfs
