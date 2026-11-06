#!/bin/bash
# First-boot provisioning for prox-lxc LXC (run as root inside the container).
set -euo pipefail

APP_USER=syarc
APP_DIR=/usr/local/syarc
DATA_DIR=/var/lib/syarc

apt-get update
apt-get install -y curl ca-certificates sqlite3 nginx

# nginx is included as TLS terminator; console itself listens on 127.0.0.1:8090.
# (Swap for your preferred proxy or direct rustls once added.)

id -u "$APP_USER" >/dev/null 2>&1 || useradd --system --home "$APP_DIR" --shell /usr/sbin/nologin "$APP_USER"

mkdir -p "$APP_DIR" "$DATA_DIR"
chown -R "$APP_USER:$APP_USER" "$APP_DIR" "$DATA_DIR"

# NFS library write access: group 2049 is mapped through the LXC idmap.
usermod -aG 2049 "$APP_USER"

cat >/etc/systemd/system/syarc-console.service <<'EOF'
[Unit]
Description=syarc console
After=network.target

[Service]
Type=simple
User=syarc
Group=syarc
WorkingDirectory=/usr/local/syarc
Environment="DATABASE_URL=sqlite:////var/lib/syarc/syarc-console.db?mode=rwc"
Environment="RUST_LOG=info"
ExecStart=/usr/local/syarc/syarc-console
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable syarc-console

echo "Provisioned. Deploy the binary to $APP_DIR/syarc-console, install the TLS"
echo "cert/key + nginx site for console.example.net, then 'systemctl start syarc-console'."
