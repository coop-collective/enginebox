#!/bin/sh
set -eu
test "$(id -u)" = 0
test -f /etc/enginebox-layerfs-validation
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl git
curl --fail --show-error --silent --location --proto '=https' --proto-redir '=https' https://get.docker.com -o /opt/layerfs-validation/docker-install.sh
sh /opt/layerfs-validation/docker-install.sh
docker version
test -c /dev/fuse || modprobe fuse
test -c /dev/fuse
