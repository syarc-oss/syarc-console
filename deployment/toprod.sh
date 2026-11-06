#!/bin/bash
# Deploy the syarc-console (Rust) to the production LXC pair.
#
# Run as syarc on server1. The release binary is built here (no Rust
# toolchain needed on the LXCs); templates ship in the git archive and
# migrations are embedded in the binary.
#
# Topology: prox-lxc1/prox-lxc2 run litefs.service; the console is exec'd by
# litefs on the primary only, with DATABASE_URL pointing at the FUSE
# mount (/var/lib/syarc/litefs/syarc-console.db). App dir on the LXCs
# is /usr/local/syarc. Deploys to BOTH nodes; restarting litefs also
# restarts the console where it is running.
#
# Usage: deployment/toprod.sh [host ...]   (default: prox-lxc1 prox-lxc2)

set -euo pipefail

DST="/usr/local/syarc"
HOSTS="${@:-prox-lxc1 prox-lxc2}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"

echo "=== Building release binary on $(hostname) ==="
cd "$HERE"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
BIN="$HERE/target/release/syarc-console"
test -x "$BIN"

TARBALL=$(mktemp /tmp/syarc-console-src-XXXXXX.tar)
git -C "$HERE" archive --format=tar HEAD > "$TARBALL"
trap 'rm -f "$TARBALL"' EXIT

for host in $HOSTS; do
  echo "=== Deploying to $host ==="
  scp -q "$TARBALL" "root@$host:/tmp/syarc-console-src.tar"
  scp -q "$BIN" "root@$host:/tmp/syarc-console.bin"

  ssh root@$host "
    set -e
    DST=$DST
    # atomically swap in the new source tree; never touch the db
    # (it lives on the litefs FUSE mount) or .env
    rm -rf \$DST.new
    mkdir \$DST.new
    tar -xf /tmp/syarc-console-src.tar -C \$DST.new
    install -m 755 /tmp/syarc-console.bin \$DST.new/syarc-console
    rm -rf \$DST.old
    [ -d \$DST ] && mv \$DST \$DST.old
    mv \$DST.new \$DST
    cp -a \$DST.old/.env \$DST/ 2>/dev/null || true
    rm -rf \$DST.old
    chown -R syarc:syarc \$DST
    systemctl restart litefs
    rm -f /tmp/syarc-console-src.tar /tmp/syarc-console.bin
    sleep 3
    systemctl --no-pager --lines=5 status litefs || true
  "
  echo "=== $host done ==="
done
