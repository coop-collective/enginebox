#!/bin/sh
# Run as root only inside a fresh, credential-free Ubuntu 24.04 CI template clone.
set -eu
export DEBIAN_FRONTEND=noninteractive
test "$(id -u)" = 0
# Supplied by the qualified Ubuntu guest.
# shellcheck disable=SC1091
. /etc/os-release
test "$ID" = ubuntu && test "$VERSION_ID" = 24.04
runner_user=${ENGINEBOX_CI_USER:-agent}
runner_home=$(getent passwd "$runner_user" | cut -d: -f6)
test -n "$runner_home" && test "$(id -u "$runner_user")" != 0
runner_uid=$(id -u "$runner_user")
runner_gid=$(id -g "$runner_user")
test "$runner_gid" != 0
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
# Keep root growth explicit and guarded in this qualified CI image. A fresh
# clone may already have grown its filesystem before this service runs.
command -v growpart >/dev/null
command -v resize2fs >/dev/null
install -o root -g root -m 0755 "$(dirname "$0")/grow-linux-root.sh" /usr/local/sbin/enginebox-ci-grow-root
cat > /etc/systemd/system/enginebox-ci-grow-root.service <<'UNIT'
[Unit]
Description=Grow the qualified disposable CI root filesystem
After=local-fs.target
Before=docker.service incus-agent.service

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/enginebox-ci-grow-root
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now enginebox-ci-grow-root.service
# Incus exec --user/--group sets only the primary group, not the account's
# supplemental groups. Keep the guest-only socket private to that primary
# group so the actual CI worker can use it without a privileged wrapper.
case $(systemctl show docker.service -p ExecStart --value) in
  *' -H fd:// '*) ;;
  *) echo 'Docker must use the qualified systemd socket activation unit' >&2; exit 1 ;;
esac
install -d -o root -g root -m 0755 /etc/systemd/system/docker.socket.d
cat > /etc/systemd/system/docker.socket.d/enginebox-ci.conf <<UNIT
[Socket]
SocketGroup=$runner_gid
SocketMode=0660
UNIT
systemctl stop docker.service docker.socket
systemctl daemon-reload
systemctl enable --now docker.socket docker.service
test "$(stat -c '%a:%u:%g' /run/docker.sock)" = "660:0:$runner_gid"
# Match the worker's numeric identity without initializing supplemental groups.
# shellcheck disable=SC2016
setpriv --reuid="$runner_uid" --regid="$runner_gid" --clear-groups \
  env HOME="$runner_home" USER="$runner_user" \
  sh -ec 'test "$(id -u)" != 0; docker info --format "Docker {{.ServerVersion}} {{.Architecture}} {{.OSType}}"; docker compose version; docker buildx version; git lfs version; sudo -n true'
enginebox-host clean-base-check --home "$runner_home"
apt-get clean
dpkg-query -W -f='${Package} ${Version}\n' docker.io docker-compose-v2 docker-buildx git-lfs
