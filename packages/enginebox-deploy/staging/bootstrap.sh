#!/usr/bin/env bash
# Create (or re-check) the Engine Box staging environment on the production host, fully isolated from production:
# its own install prefix, state root, API token, loopback listeners, systemd user unit and Incus project, with a small
# capacity budget so staging can never starve production. Idempotent: existing files are kept; re-running only fills
# in what is missing and re-verifies isolation. It reads production's fleet.toml for host facts (target, storage pool,
# network, reserves) and its release directory for an installer binary; it never writes anything under production's
# prefix, state root or Incus project.
#
# Run on the host as the controller service owner (ubuntu), from a checkout of this repository:
#   packages/enginebox-deploy/staging/bootstrap.sh --version 0.3.1-workers.41226420500 --artifacts DIR [--apply]
# DIR holds enginebox-VERSION-x86_64-unknown-linux-gnu.{json,tar.gz} (a deploy build or the controller-preview
# artifact). Without --apply it prints the plan and changes nothing.
set -euo pipefail
umask 077

STAGING=/opt/enginebox-staging
PREFIX=$STAGING/controller
STATE=$STAGING/state
PROD_PREFIX=/opt/enginebox-rust/controller
PROD_STATE=/opt/enginebox-control/state
PROD_FLEET=$PROD_PREFIX/etc/fleet.toml
PROD_PROJECT=enginebox-fleet
PROJECT=enginebox-staging
API=127.0.0.1:8865
DASHBOARD=127.0.0.1:3100
UNIT=enginebox-staging-supervisor.service
# Capacity: at most 3 workspaces plus a template/base build, 8 vCPU and 12 GiB across the staging project.
MAX_INSTANCES=3
PROJECT_INSTANCES=6
CONTRIBUTION_CPU=8
CONTRIBUTION_MEMORY_GIB=12
TARGET=x86_64-unknown-linux-gnu

VERSION= ARTIFACTS= APPLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION=$2; shift 2 ;;
    --artifacts) ARTIFACTS=$2; shift 2 ;;
    --apply) APPLY=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "--version X.Y.Z[-pre] required" >&2; exit 2; }
[ -n "$ARTIFACTS" ] && [ -d "$ARTIFACTS" ] || { echo "--artifacts DIR required" >&2; exit 2; }
MANIFEST=$ARTIFACTS/enginebox-$VERSION-$TARGET.json
ARCHIVE=$ARTIFACTS/enginebox-$VERSION-$TARGET.tar.gz
[ -f "$MANIFEST" ] && [ -f "$ARCHIVE" ] || { echo "missing $MANIFEST or $ARCHIVE" >&2; exit 2; }

# Commands never read stdin: `incus project create/set` read YAML from a non-terminal stdin (and would eat a loop's input).
run() { if [ "$APPLY" = 1 ]; then "$@" </dev/null; else printf 'would run:'; printf ' %q' "$@"; printf '\n'; fi; }
write_new() { # write_new FILE MODE: content from stdin, only when FILE does not exist yet
  local file=$1 mode=$2 content; content=$(cat)
  if [ -e "$file" ]; then echo "keep $file"; return; fi
  if [ "$APPLY" = 1 ]; then printf '%s\n' "$content" > "$file"; chmod "$mode" "$file"; echo "wrote $file"; else echo "would write $file"; fi
}

# ---- isolation guards: staging paths must not overlap production's
for p in "$PREFIX" "$STATE"; do
  case "$p" in "$PROD_PREFIX"*|"$PROD_STATE"*) echo "refusing: $p overlaps production" >&2; exit 1 ;; esac
done
[ "$PROJECT" != "$PROD_PROJECT" ] || { echo "refusing: staging Incus project equals production's" >&2; exit 1; }
if ss -ltn 2>/dev/null | awk '{print $4}' | grep -qx "$API" && ! systemctl --user is-active --quiet "$UNIT"; then
  echo "refusing: $API is already in use by something other than $UNIT" >&2; exit 1
fi

INSTALLER=$PROD_PREFIX/releases/$VERSION/bin/enginebox-install
[ -x "$INSTALLER" ] || { echo "production has no retained release $VERSION to borrow an installer binary from" >&2; exit 1; }

# ---- directories (owner-only)
run sudo install -d -o "$(id -un)" -g "$(id -gn)" -m 0700 "$STAGING"
# The installer refuses a prefix it does not own: let `stage` create it, then add etc/.
run install -d -m 0700 "$STATE"

# ---- release: stage the exact artifact into the staging prefix and activate it as the first release
if [ -d "$PREFIX/releases/$VERSION" ]; then echo "keep release $VERSION"; else
  SHA=$(sha256sum "$MANIFEST" | cut -d' ' -f1)
  run "$INSTALLER" --prefix "$PREFIX" stage --version "$VERSION" --manifest "$MANIFEST" --manifest-sha256 "$SHA" --archive "$ARCHIVE"
fi
if [ -f "$PREFIX/current.json" ]; then echo "keep active release ($(grep -o '"current": *"[^"]*"' "$PREFIX/current.json"))"; else
  run "$PREFIX/releases/$VERSION/bin/enginebox-install" --prefix "$PREFIX" activate --version "$VERSION"
fi
run install -d -m 0700 "$PREFIX/etc"

# ---- private configuration: fresh secrets, never copied from production
write_new "$PREFIX/etc/api-token" 0600 < <(openssl rand -hex 32)
write_new "$PREFIX/etc/environment.json" 0600 < <(printf '{"ENGINEBOX_DASHBOARD_PASSWORD":"%s","ENGINEBOX_SESSION_SECRET":"%s","PATH":"/usr/local/bin:/usr/bin:/bin"}' "$(openssl rand -hex 16)" "$(openssl rand -hex 32)")
write_new "$PREFIX/etc/supervisor.json" 0600 <<JSON
{
  "prefix": "$PREFIX",
  "state_dir": "$STATE",
  "fleet_config": "$PREFIX/etc/fleet.toml",
  "token_file": "$PREFIX/etc/api-token",
  "environment_file": "$PREFIX/etc/environment.json",
  "listen": "$API",
  "dashboard_listen": "$DASHBOARD",
  "drain_timeout_seconds": 300
}
JSON
# Host facts from production's fleet.toml: only the first [[hosts]] block's scalar keys (no templates, warm pools or
# other tables), with the project and instance budget replaced and contribution caps added.
FLEET=$(sudo -n cat "$PROD_FLEET" | awk -v project="$PROJECT" -v max="$MAX_INSTANCES" -v cpu="$CONTRIBUTION_CPU" -v mem="$CONTRIBUTION_MEMORY_GIB" '
  /^\[\[hosts\]\]/ { if (seen) exit; seen=1; print; next }
  /^\[/ { if (seen) exit }
  !seen { if ($0 ~ /^schema_version/) print; next }
  /^[[:space:]]*(project|max_instances|contribution_cpu|contribution_memory_gib)[[:space:]]*=/ { next }
  { print }
  END { printf "project = \"%s\"\nmax_instances = %d\ncontribution_cpu = %d\ncontribution_memory_gib = %d\n", project, max, cpu, mem }')
grep -q "^project = \"$PROJECT\"$" <<<"$FLEET" || { echo "could not derive staging fleet.toml" >&2; exit 1; }
write_new "$PREFIX/etc/fleet.toml" 0600 <<<"$FLEET"

# ---- Incus project: production's feature set and default profile, staging's own limits
if sudo -n incus project show "$PROJECT" >/dev/null 2>&1; then echo "keep Incus project $PROJECT"; else
  run sudo -n incus project create "$PROJECT"
  while IFS= read -r line; do
    key=${line%%:*}; value=${line#*: }
    case "$key" in features.*) run sudo -n incus project set "$PROJECT" "$key" "$value" ;; esac
  done < <(sudo -n incus project show "$PROD_PROJECT" | awk '/^config:/{c=1;next} /^[^ ]/{c=0} c{sub(/^  /,""); print}')
  if [ "$APPLY" = 1 ]; then sudo -n incus profile show default --project "$PROD_PROJECT" | sudo -n incus profile edit default --project "$PROJECT"; else echo "would copy the default profile from $PROD_PROJECT"; fi
fi
run sudo -n incus project set "$PROJECT" limits.instances "$PROJECT_INSTANCES"
run sudo -n incus project set "$PROJECT" limits.cpu "$CONTRIBUTION_CPU"
run sudo -n incus project set "$PROJECT" limits.memory "${CONTRIBUTION_MEMORY_GIB}GiB"

# ---- official agent clients into staging state (no provider sign-ins: see docs/STAGING.md)
run "$PREFIX/releases/$VERSION/bin/enginebox-agent-clients" --state-dir "$STATE" --apply

# ---- systemd user unit with its own name (the installer's service plan always uses enginebox-supervisor.service)
UNIT_FILE=$HOME/.config/systemd/user/$UNIT
write_new "$UNIT_FILE" 0644 <<UNITFILE
# Engine Box staging supervisor (packages/enginebox-deploy/staging/bootstrap.sh)
[Unit]
Description=Engine Box staging supervisor
After=network-online.target
[Service]
ExecStart=$PREFIX/releases/$VERSION/bin/enginebox-supervisor --config $PREFIX/etc/supervisor.json
Restart=on-failure
RestartSec=3
UMask=0077
[Install]
WantedBy=default.target
UNITFILE
run systemctl --user daemon-reload
run systemctl --user enable --now "$UNIT"
echo "staging: API http://$API, dashboard http://$DASHBOARD, Incus project $PROJECT, unit $UNIT"
echo "next: set environments/staging.json controller.installer to $PREFIX/releases/$VERSION/bin/enginebox-install if VERSION differs"
