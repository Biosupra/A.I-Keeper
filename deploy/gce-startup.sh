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
  # Run through bash: a checkout from Windows may carry no executable bit.
  if ! bash ./deploy/install.sh; then log "INSTALL FAILED"; exit 1; fi
  log "selftest"
  sudo -u aikeeper /usr/local/bin/aikeeper-env selftest || log "selftest reported problems"
  log "backfill (3 years of ETH, a year of the rest, the keeper's history, then the replay; ~60-90 min)"
  sudo -u aikeeper /usr/local/bin/aikeeper-env backfill || log "backfill had errors (the daemon keeps collecting)"
  systemctl enable aikeeper || { log "ENABLE FAILED"; exit 1; }
  touch "$DONE"
  log "first boot complete"
else
  # Every later boot: update to the newest code on GitHub when it changed. A failed build keeps the
  # running version (install.sh is only reached after a successful build).
  export PATH=/root/.cargo/bin:$PATH
  cd /opt/aikeeper-src
  git fetch --depth 1 origin main || log "git fetch failed"
  if [ "$(git rev-parse HEAD)" != "$(git rev-parse FETCH_HEAD 2>/dev/null || git rev-parse HEAD)" ] || [ ! -x /usr/local/bin/aikeeper ] || [ ! -f /etc/systemd/system/aikeeper.service ]; then
    log "update: $(git rev-parse --short HEAD) -> $(git rev-parse --short FETCH_HEAD)"
    git reset --hard FETCH_HEAD 2>/dev/null || true
    if cargo build --release; then
      systemctl stop aikeeper 2>/dev/null || true
      if bash ./deploy/install.sh; then
        systemctl enable aikeeper || log "ENABLE FAILED"
        # The history the new version reads is filled at the end of this script, with the service
        # already running again (it can take an hour or two; the A.I must not be blind meanwhile).
        UPDATED=1
        log "update complete: $(git rev-parse --short HEAD)"
      else
        log "INSTALL FAILED; keeping the running version"
      fi
    else
      log "UPDATE BUILD FAILED; keeping the running version"
    fi
  else
    log "code is current: $(git rev-parse --short HEAD)"
  fi
fi
UPDATED=${UPDATED:-0}

# Every boot: email settings from metadata (empty values leave the file's value alone).
ENV=/etc/aikeeper.env
set_var() { # name value — written double-quoted: the Gmail app password contains spaces, and
  # both bash (aikeeper-env) and systemd (EnvironmentFile) read quoted values correctly.
  [ -z "$2" ] && return
  local v="$2"; v="${v//\\/}"; v="${v//\"/}"; v="${v//|/}" # no backslash, quote or | ever reaches the file
  if grep -q "^$1=" "$ENV"; then
    sed -i "s|^$1=.*|$1=\"$v\"|" "$ENV"
  else
    echo "$1=\"$v\"" >> "$ENV"
  fi
}
if [ -f "$ENV" ]; then
  set_var AIK_SMTP_USER "$(md aik-smtp-user)"
  set_var AIK_EMAIL_TO "$(md aik-email-to)"
  set_var AIK_SMTP_PASSWORD "$(md aik-smtp-password)"
  # Claude's read of the day: the API key from its own Console workspace (metadata key aik-anthropic-key).
  set_var AIK_ANTHROPIC_API_KEY "$(md aik-anthropic-key)"
  # The website's own report (its daily digest and weekly agent test): the same token as the Worker's OPS_REPORT_TOKEN.
  set_var AIK_SITE_REPORT_TOKEN "$(md aik-site-report-token)"
  # Aero (2026-10-22, docs/AERO-VM-UPDATE.md): the gauge mode (auto | aerodrome | aero) and Aero's AERO/WETH pool.
  set_var AIK_GAUGE_MODE "$(md aik-gauge-mode)"
  set_var AIK_AERO_WETH_POOL "$(md aik-aero-weth-pool)"
  chmod 0640 "$ENV" && chgrp aikeeper "$ENV"
fi
systemctl restart aikeeper
sleep 5
log "service: $(systemctl is-active aikeeper)"

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

# After an update: fill any history the new version reads, while the service runs (both write the
# same database safely: whatever is already held is skipped, and every range is written once).
if [ "$UPDATED" = 1 ]; then
  log "update backfill started (service running)"
  sudo -u aikeeper /usr/local/bin/aikeeper-env backfill && log "update backfill complete" || log "update backfill had errors (run it again to continue)"
fi
