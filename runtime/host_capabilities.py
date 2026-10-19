"""Read-only portable-host inventory. This command does not enroll or install a node."""
from __future__ import annotations

import argparse
import csv
import io
import json
import math
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

GIB = 1024 ** 3


def probe(argv: list[str]) -> str | None:
    """Bounded, noninteractive probes; never include raw command errors in reports."""
    try:
        result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, timeout=15, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout if result.returncode == 0 else None


def positive_int(value) -> int | None:
    if isinstance(value, bool):
        return None
    try:
        number = int(value)
    except (ValueError, TypeError, OverflowError):
        return None
    return number if number > 0 else None


def label(value) -> str:
    return "".join(c for c in str(value) if c.isprintable())[:160]


def json_probe(argv: list[str]):
    value = probe(argv)
    try:
        return json.loads(value) if value else None
    except (ValueError, TypeError):
        return None


def memory_bytes(system: str) -> int | None:
    if system == "Darwin":
        return positive_int(probe(["sysctl", "-n", "hw.memsize"]))
    if system == "Windows":
        return positive_int(probe(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                                   "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"]))
    if system == "Linux":
        try:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    value = positive_int(line.split()[1])
                    return value * 1024 if value else None
        except (OSError, IndexError):
            pass
    return None


def graphics(system: str) -> dict:
    """Visible hardware is not proof of container/VM passthrough support."""
    devices = []
    if system == "Darwin":
        data = json_probe(["system_profiler", "SPDisplaysDataType", "-json"])
        if isinstance(data, dict):
            for item in data.get("SPDisplaysDataType", []):
                if isinstance(item, dict):
                    devices.append({"name": label(item.get("sppci_model", "Unknown GPU")),
                                    "source": "system_profiler", "memory_bytes": None})
        available = isinstance(data, dict)
    elif system == "Windows":
        data = json_probe(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                           "@(Get-CimInstance Win32_VideoController | Select-Object Name) | ConvertTo-Json -Compress"])
        if isinstance(data, dict):
            data = [data]
        available = isinstance(data, list)
        for item in data if available else []:
            if isinstance(item, dict):
                devices.append({"name": label(item.get("Name", "Unknown GPU")),
                                "source": "Win32_VideoController", "memory_bytes": None})
    else:
        data = probe(["nvidia-smi", "--query-gpu=index,name,memory.total", "--format=csv,noheader,nounits"])
        available = data is not None
        if data:
            for row in csv.reader(io.StringIO(data)):
                if len(row) == 3:
                    memory = positive_int(row[2].strip())
                    devices.append({"name": label(row[1].strip()), "source": "nvidia-smi",
                                    "memory_bytes": memory * 1024 ** 2 if memory else None})
        nvidia_observed = bool(devices)
        # AMD/Intel and driverless NVIDIA devices are visible through PCI sysfs.
        # Skip NVIDIA here when nvidia-smi already reported it to avoid double counting.
        pci = Path("/sys/bus/pci/devices")
        if pci.is_dir():
            available = True
            for device in sorted(pci.iterdir()):
                try:
                    if not (device / "class").read_text().strip().startswith("0x03"):
                        continue
                    vendor = (device / "vendor").read_text().strip()
                    if vendor == "0x10de" and nvidia_observed:
                        continue
                    name = {"0x10de": "NVIDIA", "0x1002": "AMD", "0x8086": "Intel"}.get(vendor, "PCI")
                    devices.append({"name": f"{name} display controller", "source": "pci-sysfs",
                                    "memory_bytes": None})
                except OSError:
                    continue
    return {"status": "observed" if available else "unknown", "devices": devices,
            "passthrough_tested": False, "schedulable": False}


def docker_inventory(enabled: bool) -> dict:
    if not enabled:
        return {"status": "not_requested"}
    # Refuse remote contexts: their CPU/RAM must not be mistaken for this laptop.
    if os.environ.get("DOCKER_HOST") and not os.environ.get("DOCKER_CONTEXT"):
        endpoint = os.environ["DOCKER_HOST"]
    else:
        endpoint = json_probe(["docker", "context", "inspect", "--format", "{{json .Endpoints.docker.Host}}"])
    if not isinstance(endpoint, str):
        return {"status": "unavailable", "reason": "Docker CLI/context is unavailable."}
    if not endpoint.startswith(("unix:///", "npipe:////./pipe/")):
        return {"status": "remote_context_refused", "reason": "Select a local Docker context before inspecting laptop capacity."}
    # Select only public capability fields; do not collect proxies, labels, names or registry configuration.
    format_string = ('{"cpus":{{.NCPU}},"memory_bytes":{{.MemTotal}},"os":{{json .OSType}},'
                     '"architecture":{{json .Architecture}},"server_version":{{json .ServerVersion}},'
                     '"security_options":{{json .SecurityOptions}}}')
    data = json_probe(["docker", "info", "--format", format_string])
    if not isinstance(data, dict):
        return {"status": "unavailable", "reason": "The local Docker engine did not return inventory."}
    return {"status": "observed", "cpus": positive_int(data.get("cpus")),
            "memory_bytes": positive_int(data.get("memory_bytes")), "os": label(data.get("os", "unknown")),
            "architecture": label(data.get("architecture", "unknown")),
            "server_version": label(data.get("server_version", "unknown")),
            "rootless": any("rootless" in str(item) for item in data.get("security_options") or [])}


def allocation(cpus: int | None, memory: int | None, disk_free: int | None,
               requested_cpu: int | None = None, requested_memory_gib: float | None = None,
               requested_disk_gib: float | None = None) -> dict:
    """Conservative personal-machine budget, not a fleet admission reservation."""
    if not cpus or not memory or disk_free is None:
        return {"status": "unknown", "reason": "CPU, RAM and disk observations are required."}
    # Preserve half the machine, at least two logical CPUs/4 GiB RAM/20 GiB free disk.
    limits = {"cpus": max(0, min(cpus // 2, cpus - 2)),
              "memory_bytes": max(0, min(memory // 2, memory - 4 * GIB)),
              "disk_bytes": max(0, min(disk_free // 2, disk_free - 20 * GIB))}
    requested = {"cpus": requested_cpu if requested_cpu is not None else limits["cpus"],
                 "memory_bytes": int(requested_memory_gib * GIB) if requested_memory_gib is not None else limits["memory_bytes"],
                 "disk_bytes": int(requested_disk_gib * GIB) if requested_disk_gib is not None else limits["disk_bytes"]}
    errors = [f"{key} exceeds the personal-machine budget or is zero."
              for key in limits if requested[key] <= 0 or requested[key] > limits[key]]
    return {"status": "rejected" if errors else "suggested", "maximum": limits,
            "requested": requested, "errors": errors, "enforced": False,
            "scope": "local machine only; Docker VM and current workload limits must also be enforced"}


def inspect_host(path: Path, *, docker: bool = False, cpus: int | None = None,
                 memory_gib: float | None = None, disk_gib: float | None = None) -> dict:
    system = platform.system()
    architecture = platform.machine().lower()
    cpu_count = os.cpu_count()
    memory = memory_bytes(system)
    try:
        disk_free = shutil.disk_usage(path).free
    except OSError:
        disk_free = None
    kvm = system == "Linux" and Path("/dev/kvm").exists()
    incus = shutil.which("incus") is not None
    eligible = system == "Linux" and architecture in {"x86_64", "amd64"} and kvm and incus
    docker_report = docker_inventory(docker)
    if docker_report.get("status") == "observed":
        docker_cpus, docker_memory = docker_report.get("cpus"), docker_report.get("memory_bytes")
        budget = allocation(min(cpu_count, docker_cpus) if cpu_count and docker_cpus else None,
                            min(memory, docker_memory) if memory and docker_memory else None,
                            disk_free, cpus, memory_gib, disk_gib)
        budget["scope"] = "bounded by both local host and Docker totals; Docker volume free space and current workloads remain unmeasured"
        docker_report["allocation"] = budget
    return {"schema_version": 1, "read_only": True,
            "machine": {"os": system, "architecture": architecture, "cpus": cpu_count,
                        "memory_bytes": memory, "disk_free_bytes": disk_free,
                        "kvm_device_present": kvm, "incus_cli_present": incus},
            "docker": docker_report, "graphics": graphics(system),
            "allocation": allocation(cpu_count, memory, disk_free, cpus, memory_gib, disk_gib),
            "backends": {
                "incus_linux_vm": {"status": "requires_doctor" if eligible else "not_eligible",
                                   "next_step": "Run ebox doctor with a reviewed host configuration." if eligible else "Requires a qualified Linux x86_64 host with Incus and usable KVM."},
                "docker_workspace": {"status": "planned", "enrollment_enabled": False},
                "native_windows": {"status": "planned", "enrollment_enabled": False},
                "native_macos": {"status": "planned", "enrollment_enabled": False}},
            "notes": ["Inventory is not host qualification. No node is enrolled and no resource limit is changed.",
                      "Docker daemon totals are separate from physical host totals; this tool does not sum them.",
                      "GPU visibility is not GPU passthrough, exclusivity or successful workload validation."]}


def positive_number(value: str) -> float:
    number = float(value)
    if not math.isfinite(number) or not 0 < number <= 1_000_000:
        raise argparse.ArgumentTypeError("Use a finite GiB value greater than zero and at most 1000000.")
    return number


def positive_cpu(value: str) -> int:
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("Use a CPU count greater than zero.")
    return number


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="Emit capability data without local paths or account details.")
    parser.add_argument("--docker", action="store_true", help="Inspect an existing local Docker engine; never start or install it.")
    parser.add_argument("--storage-path", type=Path, default=Path.cwd(), help="Existing filesystem to measure (path is not printed).")
    parser.add_argument("--cpus", type=positive_cpu, help="Validate a proposed CPU allocation.")
    parser.add_argument("--memory-gib", type=positive_number, help="Validate a proposed RAM allocation.")
    parser.add_argument("--disk-gib", type=positive_number, help="Validate a proposed disk allocation.")
    args = parser.parse_args()
    report = inspect_host(args.storage_path, docker=args.docker, cpus=args.cpus,
                          memory_gib=args.memory_gib, disk_gib=args.disk_gib)
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        machine = report["machine"]
        print(f"{machine['os']} / {machine['architecture']}: {machine['cpus']} logical CPUs")
        for field in ("memory_bytes", "disk_free_bytes"):
            value = machine[field]
            print(f"{field}: {value / GIB:.1f} GiB" if value is not None else f"{field}: unknown")
        print(f"Docker: {report['docker']['status']}; GPUs observed: {len(report['graphics']['devices'])}")
        budget = report["allocation"]
        print(f"Allocation: {budget['status']} (report only; no limits changed)")
        if budget.get("requested"):
            values = budget["requested"]
            print(f"Proposed: {values['cpus']} CPUs, {values['memory_bytes'] / GIB:.1f} GiB RAM, {values['disk_bytes'] / GIB:.1f} GiB disk")
        if report["docker"].get("allocation"):
            docker_budget = report["docker"]["allocation"]
            print(f"Docker allocation: {docker_budget['status']}")
            if docker_budget.get("requested"):
                values = docker_budget["requested"]
                print(f"Docker proposed: {values['cpus']} CPUs, {values['memory_bytes'] / GIB:.1f} GiB RAM")
        for backend, details in report["backends"].items():
            print(f"{backend}: {details['status']}")
        for note in report["notes"]:
            print(note)
    if (report["allocation"]["status"] == "rejected"
            or report["docker"].get("allocation", {}).get("status") == "rejected"):
        sys.exit(2)


if __name__ == "__main__":
    main()
