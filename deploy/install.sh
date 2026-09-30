#!/usr/bin/env bash
# Installs the A.I Keeper on a Debian VM as an unprivileged systemd service.
# Run from the repository root after `cargo build --release`:  sudo ./deploy/install.sh
set -euo pipefail

[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
[ -x target/release/aikeeper ] || { echo "build first: cargo build --release"; exit 1; }

id aikeeper >/dev/null 2>&1 || useradd --system --home /var/lib/aikeeper --create-home --shell /usr/sbin/nologin aikeeper
install -m 0755 target/release/aikeeper /usr/local/bin/aikeeper
install -d -o aikeeper -g aikeeper -m 0750 /var/lib/aikeeper

if [ ! -f /etc/aikeeper.env ]; then
  install -m 0640 -g aikeeper .env.example /etc/aikeeper.env
  sed -i 's#^AIK_DATA_DIR=.*#AIK_DATA_DIR=/var/lib/aikeeper#' /etc/aikeeper.env
  echo "created /etc/aikeeper.env — fill in the email settings"
fi

# Helper so manual commands see the same settings as the service.
cat > /usr/local/bin/aikeeper-env <<'EOF'
#!/usr/bin/env bash
set -a; . /etc/aikeeper.env; set +a
exec /usr/local/bin/aikeeper "$@"
EOF
chmod 0755 /usr/local/bin/aikeeper-env

install -m 0644 deploy/aikeeper.service /etc/systemd/system/aikeeper.service
systemctl daemon-reload
echo "installed. Next: sudo -u aikeeper /usr/local/bin/aikeeper-env selftest"
