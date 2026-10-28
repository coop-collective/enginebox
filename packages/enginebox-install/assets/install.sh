#!/bin/sh
# Engine Box terminal bootstrap. Download and inspect this file before running it.
set -eu
version=''
archive_sha256=''
manifest_sha256=''
prefix="${HOME}/.local/share/enginebox"
base_url='https://github.com/coop-collective/enginebox/releases/download'
apply=0
deps=0
role='controller'
network_profile=''
controller=''
require_quic=0
with_tunnel=0
usage() {
  cat <<'USAGE'
Usage: sh install.sh --version X.Y.Z [options]
  --archive-sha256 HEX       Optional independent archive pin (otherwise exact-release HTTPS checksum)
  --manifest-sha256 HEX      Optional independent manifest pin
  --prefix PATH             User-owned installation directory (must be new or Engine Box-owned)
  --role operator|controller|incus-host
  --apply                   Download, verify, stage and initially activate the pinned release
  --install-dependencies    With --apply, run the inspected dependency plan
  --network-profile control|streaming  Check the selected connections before installing
  --controller HTTPS_ORIGIN           Required for the control network profile
  --require-quic                       Require QUIC for a streaming/MoQ host
  --with-tunnel                        Also inspect Tunnel TCP/7844 reachability
Without --apply, prints the plan and changes nothing. Independent hashes can come from a trusted release receipt.
Running controllers are never restarted by this bootstrap; use enginebox-install update.
USAGE
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version|--archive-sha256|--manifest-sha256|--prefix|--role|--base-url|--network-profile|--controller)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case "$1" in
        --version) version=$2;; --archive-sha256) archive_sha256=$2;; --manifest-sha256) manifest_sha256=$2;;
        --prefix) prefix=$2;; --role) role=$2;; --base-url) base_url=$2;;
        --network-profile) network_profile=$2;; --controller) controller=$2;;
      esac
      shift 2;;
    --apply) apply=1; shift;; --install-dependencies) deps=1; shift;;
    --require-quic) require_quic=1; shift;; --with-tunnel) with_tunnel=1; shift;;
    --help|-h) usage; exit 0;; *) usage >&2; exit 2;;
  esac
done
case "$version" in ''|*[!0-9A-Za-z.+-]*|*..*|*+*) echo 'Provide an exact release version, without v, ranges, paths or build metadata.' >&2; exit 2;; esac
case "$role" in operator|controller|incus-host) ;; *) echo 'Unsupported role.' >&2; exit 2;; esac
case "$network_profile" in ''|control|streaming) ;; *) echo 'Unsupported network profile.' >&2; exit 2;; esac
if [ "$network_profile" = control ] && [ -z "$controller" ]; then echo 'The control network profile requires --controller.' >&2; exit 2; fi
if [ "$require_quic" -eq 1 ] && [ "$network_profile" != streaming ]; then echo '--require-quic requires --network-profile streaming.' >&2; exit 2; fi
if [ -z "$network_profile" ] && { [ -n "$controller" ] || [ "$with_tunnel" -eq 1 ]; }; then echo 'Specify --network-profile with network options.' >&2; exit 2; fi
for digest in "$archive_sha256" "$manifest_sha256"; do
  [ -n "$digest" ] || continue
  [ "${#digest}" -eq 64 ] || { echo 'SHA256 digests must contain 64 characters.' >&2; exit 2; }
  case "$digest" in *[!0-9a-f]*) echo 'SHA256 digests must be lowercase hex.' >&2; exit 2;; esac
done
case "$base_url" in https://*) ;; *) echo 'HTTPS release source required.' >&2; exit 2;; esac
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) target=x86_64-unknown-linux-gnu;; Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu;;
  Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin;; Darwin:x86_64) target=x86_64-apple-darwin;;
  *) echo 'Unsupported OS/architecture.' >&2; exit 2;;
esac
if [ "$role" = incus-host ] && [ "$(uname -s)" != Linux ]; then echo 'Incus compute requires Linux; choose operator or controller on macOS.' >&2; exit 2; fi
printf 'Engine Box %s\nTarget: %s\nPrefix: %s\nRole: %s\nSource: %s/v%s\n' "$version" "$target" "$prefix" "$role" "$base_url" "$version"
if [ "$apply" -eq 0 ]; then printf '%s\n' 'Plan only. Add --apply after reviewing the version and trusted hashes.'; exit 0; fi
command -v curl >/dev/null || { echo 'Install curl with your OS package manager, then rerun.' >&2; exit 2; }
command -v tar >/dev/null || { echo 'Install tar with your OS package manager, then rerun.' >&2; exit 2; }
if command -v sha256sum >/dev/null; then hash_tool=sha256sum; elif command -v shasum >/dev/null; then hash_tool=shasum; else echo 'Install sha256sum or shasum first.' >&2; exit 2; fi
umask 077
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
archive="$tmp/release.tar.gz"
manifest="$tmp/manifest.json"
if [ -z "$archive_sha256" ] || [ -z "$manifest_sha256" ]; then
  [ "$base_url" = 'https://github.com/coop-collective/enginebox/releases/download' ] || { echo 'Custom release sources require both independent hashes.' >&2; exit 2; }
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL --max-time 60 --max-filesize 65536 --output "$tmp/checksums" "$base_url/v$version/enginebox-$version-$target.sha256"
  if [ -z "$archive_sha256" ]; then archive_sha256=$(awk -v name="enginebox-$version-$target.tar.gz" '$2 == name {print $1}' "$tmp/checksums"); fi
  if [ -z "$manifest_sha256" ]; then manifest_sha256=$(awk -v name="enginebox-$version-$target.json" '$2 == name {print $1}' "$tmp/checksums"); fi
  for digest in "$archive_sha256" "$manifest_sha256"; do
    [ "${#digest}" -eq 64 ] || { echo 'Missing or duplicate release checksum.' >&2; exit 1; }
    case "$digest" in *[!0-9a-f]*) echo 'Malformed release checksum.' >&2; exit 1;; esac
  done
fi
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL --max-time 900 --output "$archive" "$base_url/v$version/enginebox-$version-$target.tar.gz"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL --max-time 60 --output "$manifest" "$base_url/v$version/enginebox-$version-$target.json"
hash() { if [ "$hash_tool" = shasum ]; then shasum -a 256 "$1"; else sha256sum "$1"; fi; }
actual_archive=$(hash "$archive"); actual_archive=${actual_archive%% *}
actual_manifest=$(hash "$manifest"); actual_manifest=${actual_manifest%% *}
[ "$actual_archive" = "$archive_sha256" ] && [ "$actual_manifest" = "$manifest_sha256" ] || { echo 'Release integrity check failed. Nothing installed.' >&2; exit 1; }
# Extract exactly one checksum-authenticated bootstrap executable. Rust validates every archive entry.
tar -xOf "$archive" bin/enginebox-install > "$tmp/enginebox-install"
chmod 700 "$tmp/enginebox-install"
"$tmp/enginebox-install" --prefix "$prefix" plan --role "$role"
if [ -n "$network_profile" ]; then
  set -- "$tmp/enginebox-install" --prefix "$prefix" network-check --profile "$network_profile"
  if [ -n "$controller" ]; then set -- "$@" --controller "$controller"; fi
  if [ "$require_quic" -eq 1 ]; then set -- "$@" --require-quic; fi
  if [ "$with_tunnel" -eq 1 ]; then set -- "$@" --with-tunnel; fi
  "$@"
fi
if [ "$deps" -eq 1 ]; then "$tmp/enginebox-install" --prefix "$prefix" plan --role "$role" --apply-dependencies; fi
"$tmp/enginebox-install" --prefix "$prefix" stage --version "$version" --manifest "$manifest" --manifest-sha256 "$manifest_sha256" --archive "$archive"
if [ ! -f "$prefix/current.json" ]; then "$tmp/enginebox-install" --prefix "$prefix" activate --version "$version"; fi
printf '\nStaged release: %s/releases/%s\nConfigure private etc files using supervisor.example.json, then run service --config PATH --apply.\nFor an existing controller, use update --version %s; this bootstrap never restarts it.\n' "$prefix" "$version" "$version"
