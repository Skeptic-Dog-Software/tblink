#!/bin/sh
# Install tblink's launchd jobs.
#
#   sudo ./install.sh port RDMA_IP BRIDGE_IP PEER_RDMA_IP PEER_BRIDGE_IP
#       Root LaunchDaemon on each Mac. Example for the coordinator Mac:
#       sudo ./install.sh port 10.0.1.1 10.77.0.1 10.0.1.2 10.77.0.2
#
#   ./install.sh coordinator CONFIG_JSON
#       User LaunchAgent on the oMLX coordinator Mac (not as root).
#
#   sudo ./install.sh uninstall-port | ./install.sh uninstall-coordinator
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=/usr/local/bin/tblink
PORT_LABEL=software.skepticdog.tblink.port
COORD_LABEL=software.skepticdog.tblink.coordinator

find_bin() {
  for b in "$HERE/tblink" "$HERE/target/release/tblink"; do [ -x "$b" ] && { echo "$b"; return; }; done
  echo "build first: cargo build --release" >&2; exit 1
}

case "${1:-}" in
port)
  [ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
  [ $# -eq 5 ] || { sed -n '4,6p' "$0" >&2; exit 2; }
  install -d -o root -g wheel -m 755 /usr/local/bin
  install -o root -g wheel -m 755 "$(find_bin)" "$BIN"
  P=/Library/LaunchDaemons/$PORT_LABEL.plist
  cat > "$P" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$PORT_LABEL</string>
  <key>ProgramArguments</key><array>
    <string>$BIN</string><string>port</string>
    <string>--rdma-ip</string><string>$2</string>
    <string>--bridge-ip</string><string>$3</string>
    <string>--peer</string><string>$4</string>
    <string>--peer</string><string>$5</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/var/log/tblink.log</string>
  <key>StandardErrorPath</key><string>/var/log/tblink.log</string>
</dict></plist>
EOF
  chown root:wheel "$P"; chmod 644 "$P"
  launchctl bootout system/$PORT_LABEL 2>/dev/null || true
  launchctl bootstrap system "$P"
  echo "installed $PORT_LABEL; log: /var/log/tblink.log"
  ;;
coordinator)
  [ "$(id -u)" != 0 ] || { echo "run as the logged-in user, not root" >&2; exit 1; }
  [ $# -eq 2 ] || { sed -n '8,9p' "$0" >&2; exit 2; }
  [ -x "$BIN" ] || { echo "install the port daemon first (it installs $BIN)" >&2; exit 1; }
  CFG=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
  "$BIN" coordinator --config "$CFG" --check
  mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
  P=$HOME/Library/LaunchAgents/$COORD_LABEL.plist
  cat > "$P" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$COORD_LABEL</string>
  <key>ProgramArguments</key><array>
    <string>$BIN</string><string>coordinator</string><string>--config</string><string>$CFG</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/tblink.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/tblink.log</string>
</dict></plist>
EOF
  launchctl bootout "gui/$(id -u)/$COORD_LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$P"
  echo "installed $COORD_LABEL; log: ~/Library/Logs/tblink.log"
  ;;
uninstall-port)
  launchctl bootout system/$PORT_LABEL 2>/dev/null || true
  rm -f /Library/LaunchDaemons/$PORT_LABEL.plist "$BIN"; echo "removed $PORT_LABEL" ;;
uninstall-coordinator)
  launchctl bootout "gui/$(id -u)/$COORD_LABEL" 2>/dev/null || true
  rm -f "$HOME/Library/LaunchAgents/$COORD_LABEL.plist"; echo "removed $COORD_LABEL" ;;
*) sed -n '2,13p' "$0" >&2; exit 2 ;;
esac
