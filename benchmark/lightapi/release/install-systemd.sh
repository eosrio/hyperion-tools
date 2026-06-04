#!/usr/bin/env bash
# Install the WormDB Light API + SHiP feed as systemd services for robust production operation
# (auto-restart on failure, start on boot, and the feed restarts whenever the server does).
#
# Run from the extracted preview directory:
#
#   sudo ./install-systemd.sh          # system-wide services (recommended; survive reboot)
#        ./install-systemd.sh --user   # per-user services (no root; see the linger note)
#
# Re-run any time to pick up new paths/binaries. Uninstall: ./install-systemd.sh --uninstall
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE=system
case "${1:-}" in
  --user)      MODE=user ;;
  --uninstall) MODE=uninstall ;;
  "")          MODE=system ;;
  *) echo "usage: $0 [--user|--uninstall]"; exit 2 ;;
esac

UNITS="wormdb-lightapi.service lightapi-feed.service"

# Make `systemctl --user` reachable even from a non-login shell (e.g. over SSH).
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

# ── uninstall ───────────────────────────────────────────────────────────────
if [ "$MODE" = uninstall ]; then
  if systemctl --user list-unit-files 2>/dev/null | grep -q wormdb-lightapi; then
    systemctl --user disable --now $UNITS 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/"{wormdb-lightapi,lightapi-feed}.service
    systemctl --user daemon-reload 2>/dev/null || true
    echo "removed user services"
  fi
  if [ -f /etc/systemd/system/wormdb-lightapi.service ]; then
    [ "$(id -u)" = 0 ] || { echo "system services present — re-run with sudo to remove them"; exit 1; }
    systemctl disable --now $UNITS 2>/dev/null || true
    rm -f /etc/systemd/system/{wormdb-lightapi,lightapi-feed}.service
    systemctl daemon-reload
    echo "removed system services"
  fi
  exit 0
fi

# ── resolve the run-user and bun ─────────────────────────────────────────────
if [ "$MODE" = system ]; then RUN_USER="${SUDO_USER:-root}"; else RUN_USER="$(id -un)"; fi
RUN_HOME="$(getent passwd "$RUN_USER" | cut -d: -f6)"; RUN_HOME="${RUN_HOME:-$HOME}"

BUN="$(command -v bun 2>/dev/null || true)"
[ -z "$BUN" ] && [ -x "$RUN_HOME/.local/bin/bun" ] && BUN="$RUN_HOME/.local/bin/bun"
[ -z "$BUN" ] && [ -x "$HOME/.local/bin/bun" ]      && BUN="$HOME/.local/bin/bun"
if [ -z "$BUN" ]; then
  echo "ERROR: bun not found (the feed needs it). Install:  curl -fsSL https://bun.sh/install | bash"; exit 1
fi

# ── pre-flight ───────────────────────────────────────────────────────────────
[ -x "$DIR/bin/wormdb" ]      || { echo "ERROR: $DIR/bin/wormdb missing or not executable (chmod +x bin/* ?)"; exit 1; }
[ -f "$DIR/wormdb.json" ]     || { echo "ERROR: $DIR/wormdb.json missing"; exit 1; }
[ -f "$DIR/feed/lightapi-ship-feed.js" ] || { echo "ERROR: $DIR/feed/lightapi-ship-feed.js missing"; exit 1; }
[ -f "$DIR/feed/lightapi.env" ] || { echo "ERROR: $DIR/feed/lightapi.env missing (edit it for your node first)"; exit 1; }

render() { sed -e "s#@@DIR@@#$DIR#g" -e "s#@@USER@@#$RUN_USER#g" -e "s#@@BUN@@#$BUN#g" "$1"; }

echo "WormDB Light API systemd install"
echo "  dir : $DIR"
echo "  user: $RUN_USER"
echo "  bun : $BUN"
echo "  mode: $MODE"

# ── install ──────────────────────────────────────────────────────────────────
if [ "$MODE" = system ]; then
  [ "$(id -u)" = 0 ] || { echo "ERROR: system mode needs root — re-run:  sudo $0"; exit 1; }
  render "$DIR/systemd/wormdb-lightapi.service" > /etc/systemd/system/wormdb-lightapi.service
  render "$DIR/systemd/lightapi-feed.service"   > /etc/systemd/system/lightapi-feed.service
  systemctl daemon-reload
  systemctl enable --now $UNITS
  SCTL="systemctl"; JCTL="journalctl"
else
  DEST="$HOME/.config/systemd/user"; mkdir -p "$DEST"
  # user units run as the calling user → drop User=, target default.target
  render "$DIR/systemd/wormdb-lightapi.service" | sed -e "/^User=/d" -e "s/multi-user.target/default.target/" > "$DEST/wormdb-lightapi.service"
  render "$DIR/systemd/lightapi-feed.service"   | sed -e "/^User=/d" -e "s/multi-user.target/default.target/" > "$DEST/lightapi-feed.service"
  systemctl --user daemon-reload
  systemctl --user enable --now $UNITS
  SCTL="systemctl --user"; JCTL="journalctl --user"
  if loginctl enable-linger "$RUN_USER" 2>/dev/null; then
    echo "linger enabled — services survive logout/reboot."
  else
    echo "NOTE: run once so services survive logout/reboot:  sudo loginctl enable-linger $RUN_USER"
  fi
fi

echo
echo "Installed and started. Useful commands:"
echo "  $SCTL status wormdb-lightapi lightapi-feed"
echo "  $SCTL restart lightapi-feed      # after editing feed/lightapi.env"
echo "  $JCTL -u lightapi-feed -f        # follow feed logs"
echo
$SCTL --no-pager status wormdb-lightapi.service 2>/dev/null | head -4 || true
