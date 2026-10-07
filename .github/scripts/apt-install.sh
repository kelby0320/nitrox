#!/bin/bash
# Install packages with apt, each attempt bounded and tried again: `apt-install.sh [apt-get install
# arguments]`.
#
# **Why** (2026-10-07): three CI runs that day sat in an apt step for an hour or more — the QEMU
# job's install on `main`, and on PR #367 both jobs' installs, then the QEMU job's again — while
# the display and input workflows installed the same packages beside them in seconds. A step GitHub
# cancels keeps no log, so where apt stalled could not be read. Here each attempt is cut off by
# `timeout`, which ends the step with what apt printed still in the log, and a stall is tried
# again rather than waited on for the job's six hours.
set -u
# Seconds each half may take; overridable so the retry itself can be exercised off CI.
update_secs=${APT_UPDATE_SECS:-240}
install_secs=${APT_INSTALL_SECS:-420}
for attempt in 1 2 3; do
  if timeout "$update_secs" sudo apt-get -o Acquire::Retries=3 update \
    && timeout "$install_secs" sudo apt-get -o Acquire::Retries=3 install -y "$@"; then
    exit 0
  fi
  echo "::warning::apt attempt $attempt of 3 failed or stalled"
  # An install cut off mid-way can leave packages unconfigured, which the next attempt refuses.
  sudo dpkg --configure -a || true
done
echo "::error::apt could not install: $*"
exit 1
