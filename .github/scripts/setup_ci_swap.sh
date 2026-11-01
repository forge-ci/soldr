#!/usr/bin/env bash
# Enlarge swap on GitHub-hosted Linux runners to absorb transient LLVM/LTO
# memory peaks during all-miss cross-compiles (soldr#2453).
#
# The 4-vCPU ubuntu-24.04 image ships ~16 GB RAM and only a ~4 GB swapfile.
# A cold `codegen-units=1` thin-LTO build of the daemon crates
# (`zccache-daemon-core`, `soldr-daemon`) plus the resident embedded-zccache
# daemon can momentarily exceed that ceiling and get OOM-killed. The kill
# surfaces as "compiler process was terminated by a Unix signal" and has
# moved between unrelated crates run-to-run — the signature of memory
# pressure, not a compile error.
#
# Bounding Cargo's producer queue (CARGO_BUILD_JOBS) and zccache's admission
# gate (SOLDR_JOBS) reduced the failure rate but did not eliminate it: the
# floor is a single memory-heavy rustc/LLVM child colliding with the daemon's
# resident state. Adding a large swapfile gives that transient peak somewhere
# to spill instead of tripping the OOM killer.
#
# Swap is a safety valve, not a hot path: with a low swappiness the kernel
# keeps working pages in RAM and only touches swap under genuine pressure, so
# the common (fits-in-RAM) case is unaffected while the pathological peak no
# longer kills the build.
set -euo pipefail

SIZE_GB="${CI_SWAP_GB:-14}"
# /mnt is the large ephemeral SSD on GitHub-hosted runners (~70 GB free);
# the OS disk (/) is much smaller, so the swapfile lives on /mnt.
SWAPFILE="/mnt/ci-extra-swapfile"
PROBEFILE="/mnt/ci-swap-probe"

echo "== swap before =="
free -h || true
swapon --show || true

# Preflight, because swap is OOM *headroom* and not a correctness requirement.
# Enabling swap needs CAP_SYS_ADMIN *and* a backing file on a filesystem the
# kernel can swap to. An unprivileged container (`act`, a `container:` job,
# some self-hosted setups) fails the first with EPERM; an overlayfs /mnt fails
# the second with EINVAL even under `--privileged`. A capability-bit sniff
# would pass the second case and then blow up on the real allocation, so probe
# instead: a 1 MiB file answers both questions in milliseconds and costs
# nothing when the answer is yes. Before this, `swapon` running under
# `set -euo pipefail` with no `|| true` aborted the whole job on any host that
# could not swap -- a build that would otherwise have succeeded.
probe_error=""
swap_probe_ok() {
  sudo rm -f "$PROBEFILE" || return 1
  sudo dd if=/dev/zero of="$PROBEFILE" bs=1M count=1 status=none || return 1
  sudo chmod 600 "$PROBEFILE" || return 1
  sudo mkswap "$PROBEFILE" >/dev/null 2>&1 || return 1
  # Keep the kernel's own words: "Operation not permitted" vs "Invalid
  # argument" is the whole diagnosis, and the next reader needs it in the log.
  probe_error=$(sudo swapon "$PROBEFILE" 2>&1) || return 1
  sudo swapoff "$PROBEFILE" || true
  return 0
}

if ! swap_probe_ok; then
  sudo rm -f "$PROBEFILE" || true
  # One line: a GitHub warning annotation keeps only the first line, and the
  # kernel's message is the part worth annotating.
  reason="${probe_error:-cannot create or enable a swapfile on /mnt}"
  echo "::warning::CI swap enlargement skipped: ${reason//$'\n'/ }"
  echo "Continuing without extra swap; existing swap is left untouched."
  echo "== swap after (unchanged) =="
  free -h || true
  swapon --show || true
  exit 0
fi
sudo rm -f "$PROBEFILE" || true

# Size the swapfile to what /mnt can actually spare. Recent runner images no
# longer guarantee the ~70 GB ephemeral /mnt (it can share the OS disk), and a
# fixed 14 GB dd then fills the disk and kills the job before any build starts
# ("No space left on device"). Keep a reserve for the build itself and skip
# enlargement entirely when what's left is too small to matter.
RESERVE_GB="${CI_SWAP_RESERVE_GB:-24}"
free_gb=$(df -BG --output=avail /mnt 2>/dev/null | tail -1 | tr -dc '0-9')
free_gb=${free_gb:-0}
spare_gb=$((free_gb - RESERVE_GB))
if [ "$spare_gb" -lt "$SIZE_GB" ]; then
  SIZE_GB=$spare_gb
fi
if [ "$SIZE_GB" -lt 2 ]; then
  echo "::warning::CI swap enlargement skipped: /mnt has ${free_gb} GB free, reserving ${RESERVE_GB} GB for the build"
  echo "== swap after (unchanged) =="
  free -h || true
  swapon --show || true
  exit 0
fi
echo "swapfile size: ${SIZE_GB} GB (/mnt free ${free_gb} GB, reserve ${RESERVE_GB} GB)"

# Replace any existing swap with a single large file so total swap is
# deterministic rather than "default 4 GB plus ours". Past this point the
# probe has proven swap is manageable here, so a failure is genuinely
# unexpected and stays fatal.
sudo swapoff -a || true
sudo rm -f "$SWAPFILE"

# fallocate is instant on ext4; fall back to dd if the filesystem rejects it.
if ! sudo fallocate -l "${SIZE_GB}G" "$SWAPFILE" 2>/dev/null; then
  sudo dd if=/dev/zero of="$SWAPFILE" bs=1M count=$((SIZE_GB * 1024)) status=none
fi
sudo chmod 600 "$SWAPFILE"
sudo mkswap "$SWAPFILE" >/dev/null
sudo swapon "$SWAPFILE"

# Keep pages in RAM until real pressure hits (10 = spill late, stay fast).
sudo sysctl -w vm.swappiness=10 >/dev/null || true

echo "== swap after =="
free -h || true
swapon --show || true
