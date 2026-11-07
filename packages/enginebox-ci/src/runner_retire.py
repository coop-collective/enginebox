"""Guest-side idle retirement fence; assigned jobs retain their full deadline."""
import fcntl
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
assert root.is_dir() and not root.is_symlink()
with (root / "lock").open("a") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    if (root / "started.json").exists():
        status = "busy"
    else:
        (root / "retired").write_text("administrator_retirement")
        status = "requested"
print(json.dumps({"reserve_retire": status}))
