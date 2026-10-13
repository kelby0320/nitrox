#!/bin/bash
# Phase 6 Part H.3, the Linux half: h3.nx's four sets under Linux, on the same laptop, stick and
# disk (docs/planning/phase-6-usb.md, Part H). `cargo xtask throughput-stick` puts this file on the
# stick as linux.sh. From a Linux live USB, with the stick mounted (a desktop mounts it by itself):
#
#     lsblk -f                                         # find Nitrox's root partition: ext4
#     sudo bash /media/$USER/THROUGHPUT/linux.sh /dev/sdXN
#
# It mounts that partition, copies and removes in a scratch directory on it as the sets do in
# /home, and writes what it measured to RESULTS/ on the stick. Each step is timed with a `sync`
# inside its time and the page cache dropped before it, which stands in for h3.nx's fresh boots.
# The scratch directory goes at the end, so the disk is left as Nitrox had it.
set -euo pipefail

root=${1:-}
stick=$(cd "$(dirname "$0")" && pwd)
if [ "$(id -u)" != 0 ]; then
    echo "linux.sh: run it with sudo: it mounts a partition and drops the page cache" >&2
    exit 1
fi
if [ -z "$root" ] || [ ! -b "$root" ]; then
    echo "linux.sh: name Nitrox's root partition, the ext4 one \`lsblk -f\` shows: linux.sh /dev/sdXN" >&2
    exit 1
fi
if [ "$(blkid -o value -s TYPE "$root")" != ext4 ]; then
    echo "linux.sh: $root is not ext4, so it is not Nitrox's root" >&2
    exit 1
fi
for d in big small tree; do
    if [ ! -d "$stick/$d" ]; then
        echo "linux.sh: $stick/$d is missing: is this the throughput stick?" >&2
        exit 1
    fi
    if [ -e "$stick/c-$d" ]; then
        echo "linux.sh: $stick/c-$d is left from a run: h3.nx's set 4 removes it" >&2
        exit 1
    fi
done

mnt=$(mktemp -d /tmp/nitrox-root.XXXXXX)
mount "$root" "$mnt"
trap 'umount "$mnt"; rmdir "$mnt"' EXIT
scratch="$mnt/h3-linux"
if [ -e "$scratch" ]; then
    echo "linux.sh: $scratch is left from a run; remove it first" >&2
    exit 1
fi
mkdir "$scratch"
# Only what this run made is removed, and on any exit: a failed step leaves no scratch behind.
trap 'rm -rf "$scratch"; umount "$mnt"; rmdir "$mnt"' EXIT

mkdir -p "$stick/RESULTS"
out="$stick/RESULTS/linux-$(date +%Y%m%d-%H%M%S).txt"
{
    echo "Linux $(uname -r), $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "stick: $(findmnt -no SOURCE,OPTIONS --target "$stick")"
    echo "disk:  $root $(findmnt -no OPTIONS "$mnt")"
    lsblk -o NAME,MODEL,TRAN,SIZE,FSTYPE,LABEL
    echo
} > "$out"

# One step: the page cache dropped, then the command and a `sync`, timed together.
step() {
    local what=$1
    shift
    sync
    echo 3 > /proc/sys/vm/drop_caches
    local t0 t1
    t0=$(date +%s%N)
    "$@"
    sync
    t1=$(date +%s%N)
    awk -v w="$what" -v a="$t0" -v b="$t1" 'BEGIN { printf "%-34s %9.2f s\n", w, (b - a) / 1e9 }' | tee -a "$out"
}

for d in big small tree; do step "set 1: stick -> disk  $d" cp -r "$stick/$d" "$scratch/t-$d"; done
for d in big small tree; do step "set 2: disk -> disk   $d" cp -r "$scratch/t-$d" "$scratch/u-$d"; done
for d in big small tree; do step "set 3: disk -> stick  $d" cp -r "$scratch/t-$d" "$stick/c-$d"; done
for d in big small tree; do step "set 4: remove on disk  u-$d" rm -r "$scratch/u-$d"; done
for d in big small tree; do step "set 4: remove on stick c-$d" rm -r "$stick/c-$d"; done

echo "linux.sh: done; the times are in $out"
