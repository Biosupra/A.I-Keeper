#!/usr/bin/env bash
# Google Compute Engine startup script for the A.I Keeper VM. Runs as root at EVERY boot:
#   * first boot: swap, Rust, build from GitHub, install the service, backfill history, start;
#   * later boots: pull the newest code from GitHub and rebuild when it changed (so an update is
#     "push, then restart the VM");
#   * every boot: copy email settings from the VM's metadata into /etc/aikeeper.env and restart,
#     so the Gmail app password can be added in the Cloud console (VM -> Edit -> Metadata ->
#     key `aik-smtp-password`) and never has to travel through chat or git.
# Progress: sudo journalctl -u google-startup-scripts  (or the serial console).
set -uo pipefail
log() { echo "[aikeeper-setup] $*"; }
MD=http://metadata.google.internal/computeMetadata/v1/instance/attributes
md() { curl -sf -H "Metadata-Flavor: Google" "$MD/$1" || true; }

DONE=/var/lib/aikeeper-setup-done
if [ ! -f "$DONE" ]; then
  log "first boot: installing"
  if ! swapon --show | grep -q /swapfile; then
    fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
    grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
  fi
  apt-get update -y && DEBIAN_FRONTEND=noninteractive apt-get install -y git build-essential pkg-config curl ca-certificates
  if [ ! -x /root/.cargo/bin/cargo ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
  export PATH=/root/.cargo/bin:$PATH
  rm -rf /opt/aikeeper-src && git clone --depth 1 https://github.com/Biosupra/A.I-Keeper.git /opt/aikeeper-src
  cd /opt/aikeeper-src
  log "building (15-30 min on an e2-micro)"
  if ! cargo build --release; then log "BUILD FAILED"; exit 1; fi
  ./deploy/install.sh
  log "selftest"
  sudo -u aikeeper /usr/local/bin/aikeeper-env selftest || true
  log "backfill (a year of prices, the keeper's history; ~45 min)"
  sudo -u aikeeper /usr/local/bin/aikeeper-env backfill --days 365 || log "backfill had errors (the daemon keeps collecting)"
  systemctl enable aikeeper
  touch "$DONE"
  log "first boot complete"
else
  # Every later boot: update to the newest code on GitHub when it changed. A failed build keeps the
  # running version (install.sh is only reached after a successful build).
  export PATH=/root/.cargo/bin:$PATH
  cd /opt/aikeeper-src
  if git fetch --depth 1 origin main && [ "$(git rev-parse HEAD)" != "$(git rev-parse FETCH_HEAD)" ]; then
    log "update: $(git rev-parse --short HEAD) -> $(git rev-parse --short FETCH_HEAD)"
    git reset --hard FETCH_HEAD
    if cargo build --release; then
      systemctl stop aikeeper || true
      ./deploy/install.sh
      # Pick up anything the new version reads from history (cheap: stored files are skipped).
      sudo -u aikeeper /usr/local/bin/aikeeper-env backfill --days 3 || log "update backfill had errors"
      log "update complete: $(git rev-parse --short HEAD)"
    else
      log "UPDATE BUILD FAILED; keeping the running version"
    fi
  else
    log "code is current: $(git rev-parse --short HEAD)"
  fi
fi

# Every boot: email settings from metadata (empty values leave the file's value alone).
ENV=/etc/aikeeper.env
set_var() { # name value
  [ -z "$2" ] && return
  if grep -q "^$1=" "$ENV"; then
    # Values never contain '|'; Gmail app passwords are letters and spaces.
    sed -i "s|^$1=.*|$1=$2|" "$ENV"
  else
    echo "$1=$2" >> "$ENV"
  fi
}
if [ -f "$ENV" ]; then
  set_var AIK_SMTP_USER "$(md aik-smtp-user)"
  set_var AIK_EMAIL_TO "$(md aik-email-to)"
  set_var AIK_SMTP_PASSWORD "$(md aik-smtp-password)"
  chmod 0640 "$ENV" && chgrp aikeeper "$ENV"
fi
systemctl restart aikeeper
log "service restarted: $(systemctl is-active aikeeper)"

# A one-off test brief: set metadata `aik-send-test-brief` to any new value and restart the VM.
TOKEN="$(md aik-send-test-brief)"
if [ -n "$TOKEN" ] && [ ! -f "/var/lib/aikeeper-test-brief-$TOKEN" ]; then
  if sudo -u aikeeper /usr/local/bin/aikeeper-env brief --send; then
    touch "/var/lib/aikeeper-test-brief-$TOKEN"
    log "test brief sent ($TOKEN)"
  else
    log "TEST BRIEF FAILED ($TOKEN)"
  fi
fi
