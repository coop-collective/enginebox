#!/bin/sh
# Grow only the qualified Ubuntu CI image layout; never format or recreate a disk.
set -eu
fail() { printf '%s\n' "CI root expansion refused: $*" >&2; exit 1; }
test "$(id -u)" = 0 || fail 'root required'
os=$(cat /etc/os-release)
printf '%s\n' "$os" | grep -qx 'ID=ubuntu' || fail 'expected Ubuntu'
printf '%s\n' "$os" | grep -qx 'VERSION_ID="24.04"' || fail 'expected Ubuntu 24.04'
test "$(findmnt -n -o SOURCE,FSTYPE /)" = '/dev/sda2 ext4' || fail 'expected /dev/sda2 ext4 root'
test "$(lsblk -dnro TYPE /dev/sda)" = disk || fail 'expected plain root disk'
test "$(lsblk -nro NAME /dev/sda | tr '\n' ' ')" = 'sda sda1 sda2 ' || fail 'unexpected partition layout'
test "$(lsblk -nro FSTYPE /dev/sda1)" = vfat || fail 'expected EFI first partition'
disk_bytes=$(blockdev --getsize64 /dev/sda)
root_bytes=$(blockdev --getsize64 /dev/sda2)
test "$disk_bytes" -gt "$root_bytes" || fail 'invalid disk/root geometry'
if change=$(growpart /dev/sda 2 2>&1); then
  printf '%s\n' "$change"
else
  # growpart returns 1 when the partition already fills the disk.
  case "$change" in NOCHANGE:*) printf '%s\n' "$change" ;; *) fail "$change" ;; esac
fi
resize2fs /dev/sda2
