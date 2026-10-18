#!/usr/bin/env bash
# Run as root inside an Engine Box Ubuntu guest. Never logs in to providers.
set -euo pipefail
umask 022
export DEBIAN_FRONTEND=noninteractive
EBOX_USER=${EBOX_USER:-agent}
EBOX_UID=${EBOX_UID:-1001}
EBOX_GID=${EBOX_GID:-1001}
EBOX_HOME=${EBOX_HOME:-/home/$EBOX_USER}
EBOX_WORKSPACE=${EBOX_WORKSPACE:-/workspace}
EBOX_INSTALL_CODEX=${EBOX_INSTALL_CODEX:-1}
EBOX_INSTALL_CLAUDE=${EBOX_INSTALL_CLAUDE:-1}
EBOX_INSTALL_CURSOR=${EBOX_INSTALL_CURSOR:-1}
EBOX_MCP=${EBOX_MCP:-1}
EBOX_BROWSER=${EBOX_BROWSER:-1}
EBOX_DESKTOP=${EBOX_DESKTOP:-0}
EBOX_COMPUTER_MCP=${EBOX_COMPUTER_MCP:-$EBOX_DESKTOP}
EBOX_UPDATE_CLIENTS=${EBOX_UPDATE_CLIENTS:-0}
EBOX_CODEX_VERSION=${EBOX_CODEX_VERSION:-0.163.0}
EBOX_CLAUDE_VERSION=${EBOX_CLAUDE_VERSION:-2.1.302}
EBOX_NODE_VERSION=${EBOX_NODE_VERSION:-22.23.3}
EBOX_PLAYWRIGHT_MCP_VERSION=${EBOX_PLAYWRIGHT_MCP_VERSION:-0.0.83}
export EBOX_USER EBOX_UID EBOX_GID EBOX_HOME EBOX_WORKSPACE EBOX_DESKTOP EBOX_BROWSER
export EBOX_INSTALL_CODEX EBOX_INSTALL_CLAUDE EBOX_INSTALL_CURSOR EBOX_MCP EBOX_COMPUTER_MCP
export EBOX_UPDATE_CLIENTS
export EBOX_CODEX_VERSION EBOX_CLAUDE_VERSION EBOX_NODE_VERSION EBOX_PLAYWRIGHT_MCP_VERSION

test "$(id -u)" = 0 || { echo 'Run guest_setup.sh as root inside the guest.' >&2; exit 1; }
python3 - <<'PY'
import os, re
from pathlib import PurePosixPath
def require(condition, message):
    if not condition: raise SystemExit(message)
require(re.fullmatch(r'[a-z_][a-z0-9_-]{0,31}', os.environ['EBOX_USER']) and os.environ['EBOX_USER'] != 'root', 'Invalid guest username')
for name in ('EBOX_UID', 'EBOX_GID'):
    require(os.environ[name].isdigit() and 0 < int(os.environ[name]) < 4294967295, f'Invalid {name}')
for name in ('EBOX_HOME', 'EBOX_WORKSPACE'):
    value = os.environ[name]
    require(value.startswith('/') and value != '/' and '..' not in PurePosixPath(value).parts and not any(ord(c) < 32 or ord(c) == 127 for c in value), f'Invalid {name}')
for name in ('EBOX_DESKTOP', 'EBOX_BROWSER', 'EBOX_MCP', 'EBOX_COMPUTER_MCP', 'EBOX_INSTALL_CODEX', 'EBOX_INSTALL_CLAUDE', 'EBOX_INSTALL_CURSOR', 'EBOX_UPDATE_CLIENTS'):
    require(os.environ[name] in ('0', '1'), f'{name} must be 0 or 1')
require(os.environ['EBOX_COMPUTER_MCP'] != '1' or os.environ['EBOX_DESKTOP'] == '1', 'Computer MCP requires the desktop')
for name in ('EBOX_CODEX_VERSION', 'EBOX_CLAUDE_VERSION', 'EBOX_NODE_VERSION', 'EBOX_PLAYWRIGHT_MCP_VERSION'):
    require(re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?', os.environ[name]), f'Invalid pinned {name}')
PY
# Standard Ubuntu identification, read inside the guest.
# shellcheck disable=SC1091
. /etc/os-release
test "$ID" = ubuntu && test "$VERSION_ID" = 24.04 || { echo 'Guest setup currently supports Ubuntu 24.04 only.' >&2; exit 1; }
case "$(uname -m)" in x86_64) task_arch=x64 ;; aarch64) task_arch=arm64 ;; *) echo 'Unsupported guest architecture.' >&2; exit 1 ;; esac
test -f /opt/enginebox-runtime/browser_tools.py
test -f /opt/enginebox-runtime/requirements.lock
apt-get update -qq
apt-get install -y --no-install-recommends ca-certificates curl git ripgrep jq python3 python3-venv build-essential tmux xz-utils apparmor

if id "$EBOX_USER" >/dev/null 2>&1; then
  test "$(id -u "$EBOX_USER")" = "$EBOX_UID" && test "$(id -g "$EBOX_USER")" = "$EBOX_GID" || { echo 'Existing guest user has a different UID/GID; refusing to change it.' >&2; exit 1; }
  test "$(getent passwd "$EBOX_USER" | cut -d: -f6)" = "$EBOX_HOME" || { echo 'Existing guest user has a different home.' >&2; exit 1; }
else
  getent group "$EBOX_GID" >/dev/null || groupadd --gid "$EBOX_GID" "$EBOX_USER"
  useradd --create-home --uid "$EBOX_UID" --gid "$EBOX_GID" --home-dir "$EBOX_HOME" --shell /bin/bash "$EBOX_USER"
fi
install -d -o "$EBOX_UID" -g "$EBOX_GID" -m 0700 "$EBOX_HOME"
install -d -o "$EBOX_UID" -g "$EBOX_GID" "$EBOX_WORKSPACE" \
  "$EBOX_HOME/.local" "$EBOX_HOME/.local/share" "$EBOX_HOME/.local/bin" "$EBOX_HOME/.cache"

# Each Incus disk clone receives a new virtual machine UUID and MAC address.
# DHCP uses MAC immediately; filesystem identity rotates before networking.
install -d /usr/local/libexec /var/lib/enginebox
cat >/usr/local/libexec/enginebox-identity.py <<'PY'
#!/usr/bin/python3
import os, re, shutil, socket, subprocess
from pathlib import Path
identity = Path('/sys/class/dmi/id/product_uuid').read_text().strip().lower().replace('-', '')
if not re.fullmatch(r'[0-9a-f]{32}', identity) or identity == '0' * 32:
    raise SystemExit('No usable VM UUID; refusing to invent a shared machine identity')
marker = Path('/var/lib/enginebox/instance-uuid')
if marker.exists() and marker.read_text().strip() == identity:
    raise SystemExit(0)
Path('/etc/machine-id').write_text(identity + '\n')
dbus = Path('/var/lib/dbus/machine-id')
if dbus.exists() and not dbus.is_symlink(): dbus.write_text(identity + '\n')
hostname = 'ebox-' + identity[:12]
Path('/etc/hostname').write_text(hostname + '\n')
socket.sethostname(hostname.encode())
hosts = Path('/etc/hosts')
lines = [line for line in hosts.read_text().splitlines() if not re.match(r'^127\.0\.1\.1\s', line)]
hosts.write_text('\n'.join([*lines, '127.0.1.1\t' + hostname]) + '\n')
if shutil.which('ssh-keygen'):
    for key in Path('/etc/ssh').glob('ssh_host_*_key*'): key.unlink()
    subprocess.run(['ssh-keygen', '-A'], check=True, stdout=subprocess.DEVNULL)
temporary = marker.with_suffix('.tmp')
temporary.write_text(identity + '\n')
temporary.replace(marker)
PY
chmod 0755 /usr/local/libexec/enginebox-identity.py
cat >/etc/systemd/system/enginebox-identity.service <<'UNIT'
[Unit]
Description=Engine Box clone identity
DefaultDependencies=no
After=local-fs.target
Before=network-pre.target systemd-networkd.service NetworkManager.service ssh.service
Wants=network-pre.target
[Service]
Type=oneshot
ExecStart=/usr/local/libexec/enginebox-identity.py
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
UNIT
cat >/etc/netplan/99-enginebox.yaml <<'YAML'
network:
  version: 2
  ethernets:
    enginebox:
      match:
        name: en*
      dhcp4: true
      dhcp-identifier: mac
YAML
chmod 0600 /etc/netplan/99-enginebox.yaml
systemctl daemon-reload
systemctl enable enginebox-identity.service
# Seed the current UUID so rerunning setup does not rotate the same VM's keys.
if ! test -f /var/lib/enginebox/instance-uuid; then
  tr -d '\n-' </sys/class/dmi/id/product_uuid | tr '[:upper:]' '[:lower:]' >/var/lib/enginebox/instance-uuid
fi

# Do not inherit the operator's credential variables into installation commands.
as_guest() {
  # Change out of root's private cwd before invoking provider installers. Pass
  # argv literally and leave stdin connected for the Python configuration steps.
  # shellcheck disable=SC2016
  runuser -u "$EBOX_USER" -- env -i HOME="$EBOX_HOME" USER="$EBOX_USER" LOGNAME="$EBOX_USER" \
    PATH="$EBOX_HOME/.local/bin:/usr/local/bin:/usr/bin:/bin" SHELL=/bin/bash \
    DISABLE_AUTOUPDATER=1 DISPLAY=:99 /bin/bash -c 'cd "$HOME" && exec "$@"' -- "$@"
}
task_tmp=$(mktemp -d)
trap 'rm -rf "$task_tmp"' EXIT
chmod 0755 "$task_tmp"
if ! command -v node >/dev/null || ! node -e 'process.exit(Number(process.versions.node.split(".")[0]) >= 18 ? 0 : 1)'; then
  task_archive="node-v$EBOX_NODE_VERSION-linux-$task_arch.tar.xz"
  curl -fsSL "https://nodejs.org/dist/v$EBOX_NODE_VERSION/SHASUMS256.txt" -o "$task_tmp/SHASUMS256.txt"
  curl -fsSL "https://nodejs.org/dist/v$EBOX_NODE_VERSION/$task_archive" -o "$task_tmp/$task_archive"
  (cd "$task_tmp" && grep " $task_archive$" SHASUMS256.txt | sha256sum -c -)
  tar -xJf "$task_tmp/$task_archive" -C /usr/local --strip-components=1
fi

install_client() {
  local enabled=$1 command_name=$2 url=$3 interpreter=$4
  shift 4
  test "$enabled" = 1 || return 0
  # $1 belongs to the non-root child shell.
  # shellcheck disable=SC2016
  if as_guest bash -c 'command -v "$1" >/dev/null && "$1" --version >/dev/null 2>&1' -- "$command_name" && test "$EBOX_UPDATE_CLIENTS" != 1; then
    echo "Preserving installed $command_name; set EBOX_UPDATE_CLIENTS=1 for an explicit reinstall."
    return 0
  fi
  curl -fsSL "$url" -o "$task_tmp/$command_name-install"
  chmod 0644 "$task_tmp/$command_name-install"
  as_guest env CODEX_NON_INTERACTIVE=1 "$interpreter" "$task_tmp/$command_name-install" "$@"
  as_guest "$command_name" --version >/dev/null
}
install_client "$EBOX_INSTALL_CODEX" codex https://chatgpt.com/codex/install.sh sh --release "$EBOX_CODEX_VERSION"
install_client "$EBOX_INSTALL_CLAUDE" claude https://claude.ai/install.sh bash "$EBOX_CLAUDE_VERSION"
install_client "$EBOX_INSTALL_CURSOR" agent https://cursor.com/install bash

# Shell startup setup contains no credentials and does not modify user rc files.
python3 - <<'PY'
import os
from pathlib import Path
text = 'export PATH="$HOME/.local/bin:$PATH"\n'
text += 'export DISABLE_AUTOUPDATER=1\n'
text += 'export DISPLAY=:99\n' if os.environ['EBOX_DESKTOP'] == '1' else ''
Path('/etc/profile.d/enginebox-native.sh').write_text(text)
PY

if test "$EBOX_MCP" = 1; then
  test -x /opt/enginebox-runtime/.venv/bin/python || python3 -m venv /opt/enginebox-runtime/.venv
  /opt/enginebox-runtime/.venv/bin/pip install --disable-pip-version-check --require-hashes -r /opt/enginebox-runtime/requirements.lock
fi

if test "$EBOX_BROWSER" = 1; then
  task_browser="/opt/enginebox-browser/releases/$EBOX_PLAYWRIGHT_MCP_VERSION"
  install -d "$task_browser" /opt/enginebox-browser/chromium
  if ! test -f "$task_browser/ready"; then
    npm install --prefix "$task_browser" --save-exact --omit=dev --no-audit --no-fund "@playwright/mcp@$EBOX_PLAYWRIGHT_MCP_VERSION"
    PLAYWRIGHT_BROWSERS_PATH=/opt/enginebox-browser/chromium "$task_browser/node_modules/.bin/playwright" install --with-deps chromium
    PLAYWRIGHT_BROWSERS_PATH=/opt/enginebox-browser/chromium node -e \
      'console.log(require(process.argv[1]).chromium.executablePath())' "$task_browser/node_modules/playwright" > "$task_browser/chromium-path.txt"
    test -x "$(cat "$task_browser/chromium-path.txt")"
    touch "$task_browser/ready"
  fi
  ln -sfn "$task_browser" /opt/enginebox-browser/current
  # Ubuntu 24.04 restricts unprivileged user namespaces to approved executables.
  # Permit only the installed Chromium executable; never disable the global policy.
  if command -v apparmor_parser >/dev/null && test -d /sys/kernel/security/apparmor; then
    task_chromium=$(cat "$task_browser/chromium-path.txt")
    cat >/etc/apparmor.d/enginebox-chromium <<EOF
abi <abi/4.0>,
include <tunables/global>
profile enginebox-chromium "$task_chromium" flags=(unconfined) {
  userns,
}
EOF
    apparmor_parser -r /etc/apparmor.d/enginebox-chromium
  fi
fi

if test "$EBOX_DESKTOP" = 1; then
  apt-get install -y --no-install-recommends xvfb openbox xterm dbus-x11 scrot xdotool x11vnc novnc websockify fonts-dejavu-core
  python3 - <<'PY'
import os
from pathlib import Path
u, h = os.environ['EBOX_USER'], os.environ['EBOX_HOME']
h = h.replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%')
services = {
 'x': ('/usr/bin/Xvfb :99 -screen 0 1440x900x24 -nolisten tcp', ''),
 'desktop': ('/usr/bin/dbus-run-session -- /usr/bin/openbox-session', 'Requires=enginebox-x.service\nAfter=enginebox-x.service\n'),
 'vnc': ('/usr/bin/x11vnc -display :99 -localhost -forever -shared -nopw -rfbport 5900', 'Requires=enginebox-x.service\nAfter=enginebox-x.service\n'),
 'web': ('/usr/bin/websockify --web=/usr/share/novnc 127.0.0.1:6080 localhost:5900', 'Requires=enginebox-vnc.service\nAfter=enginebox-vnc.service\n'),
}
for name, (command, dependencies) in services.items():
    Path(f'/etc/systemd/system/enginebox-{name}.service').write_text(
        f'[Unit]\nDescription=Engine Box {name}\n{dependencies}[Service]\nUser={u}\n'
        f'Environment="HOME={h}"\nEnvironment=DISPLAY=:99\nExecStart={command}\nRestart=on-failure\n'
        '[Install]\nWantedBy=multi-user.target\n')
PY
  systemctl daemon-reload
  systemctl enable --now enginebox-x enginebox-desktop enginebox-vnc enginebox-web
fi

# Add only missing Engine Box MCP entries. Preserve existing entries, auth and
# unrelated settings. An existing entry with this name belongs to the operator.
if test "$EBOX_MCP" = 1; then
  as_guest python3 - "$EBOX_COMPUTER_MCP" "$EBOX_BROWSER" "$EBOX_INSTALL_CODEX" "$EBOX_INSTALL_CLAUDE" "$EBOX_INSTALL_CURSOR" <<'PY'
import json, os, subprocess, sys, tomllib
from pathlib import Path
desktop, browser, codex, claude, cursor = [v == '1' for v in sys.argv[1:]]
home = Path.home()
def load(path):
    return json.loads(path.read_text()) if path.exists() else {}
codex_path = home / '.codex/config.toml'
codex_config = tomllib.loads(codex_path.read_text()) if codex_path.exists() else {}
claude_config = load(home / '.claude.json')
cursor_path = home / '.cursor/mcp.json'
cursor_config = load(cursor_path)
changed = False
for name in (['enginebox-computer'] if desktop else []) + (['enginebox-browser'] if browser else []):
    for client, enabled in [('codex', codex), ('claude', claude), ('cursor', cursor)]:
        if not enabled:
            continue
        if name == 'enginebox-computer':
            command = ['/opt/enginebox-runtime/.venv/bin/python', '/opt/enginebox-runtime/mcp_server.py']
            environment = {'ENGINEBOX_GUEST': '1'}
        else:
            command = ['/usr/bin/python3', '/opt/enginebox-runtime/browser_tools.py', '--client', client]
            environment = {}
        if client == 'codex' and name not in codex_config.get('mcp_servers', {}):
            args = ['codex', 'mcp', 'add', name]
            for key, value in environment.items(): args += ['--env', f'{key}={value}']
            subprocess.run([*args, '--', *command], check=True)
        elif client == 'claude' and name not in claude_config.get('mcpServers', {}):
            args = ['claude', 'mcp', 'add', '--scope', 'user', name]
            for key, value in environment.items(): args += ['--env', f'{key}={value}']
            subprocess.run([*args, '--', *command], check=True)
        elif client == 'cursor' and name not in cursor_config.get('mcpServers', {}):
            cursor_config.setdefault('mcpServers', {})[name] = {'command': command[0], 'args': command[1:], 'env': environment}
            changed = True
if changed:
    cursor_path.parent.mkdir(mode=0o700, exist_ok=True)
    temporary = cursor_path.with_suffix('.enginebox-tmp')
    temporary.write_text(json.dumps(cursor_config, indent=2) + '\n')
    temporary.chmod(0o600)
    temporary.replace(cursor_path)
PY
fi

# Version output is safe to record; no auth/status command runs during setup.
as_guest python3 - "$EBOX_USER" "$EBOX_PLAYWRIGHT_MCP_VERSION" <<'PY' >/opt/enginebox-runtime/installed-versions.json
from datetime import datetime, timezone
import json, platform, shutil, subprocess, sys
versions = {}
for command in ('codex', 'claude', 'agent', 'node', 'npm'):
    if shutil.which(command):
        result = subprocess.run([command, '--version'], capture_output=True, text=True, timeout=30)
        versions[command] = {'version': result.stdout.strip(), 'returncode': result.returncode}
manifest = {'recorded_at': datetime.now(timezone.utc).isoformat(), 'user': sys.argv[1], 'architecture': platform.machine(), 'clients': versions}
from pathlib import Path
browser = Path('/opt/enginebox-browser/current/node_modules/@playwright/mcp/package.json')
if browser.exists(): manifest['playwright_mcp'] = json.loads(browser.read_text())['version']
print(json.dumps(manifest, indent=2))
PY
# Provisioning runs after initial image startup, never as a cloud-init runcmd.
# Keep the rendered netplan and clone identity service, but avoid repeating
# metadata discovery/configuration on every prepared-image clone boot.
if command -v cloud-init >/dev/null && ! test -f /etc/cloud/cloud-init.disabled; then
  timeout 180 cloud-init status --wait
  install -m 0644 /dev/null /etc/cloud/cloud-init.disabled
fi
echo 'Guest preparation complete. Owners sign in through each native CLI; no provider authentication was performed.'
