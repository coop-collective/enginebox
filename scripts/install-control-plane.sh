#!/usr/bin/env bash
# Plans by default. Installation requires an explicit --apply.
set -euo pipefail
task_script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$task_script_dir/control-plane/install.py" "$@"
