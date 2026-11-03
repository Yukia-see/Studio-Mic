#!/usr/bin/env bash
# Studio Mic - uninstaller. Your settings in ~/.config/studio-mic are kept.
#   ./uninstall.sh           remove the user install
#   sudo ./uninstall.sh --system   remove the /usr/local install
set -euo pipefail

BIN_NAME="studiomic"
MODE="user"
[ "${1:-}" = "--system" ] && MODE="system"
if [ "$(id -u)" -eq 0 ]; then
    MODE="system"
    TARGET_USER="${SUDO_USER:-root}"
else
    TARGET_USER="$(id -un)"
fi
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
as_user() { if [ "$(id -u)" -eq 0 ] && [ "$TARGET_USER" != "root" ]; then runuser -u "$TARGET_USER" -- env XDG_RUNTIME_DIR="/run/user/$(id -u "$TARGET_USER")" "$@"; else "$@"; fi; }

if [ "$MODE" = "system" ]; then
    BIN_DIR="/usr/local/bin"; DESKTOP_DIR="/usr/share/applications"; ICON_DIR="/usr/share/icons/hicolor/scalable/apps"
    PW_DROPIN="/usr/share/pipewire/pipewire.conf.d/60-studio-mic-buses.conf"; UNIT="/usr/lib/systemd/user/studiomic.service"
    SHARE_DIR="/usr/local/share/studio-mic"
else
    BIN_DIR="$TARGET_HOME/.local/bin"; DESKTOP_DIR="$TARGET_HOME/.local/share/applications"; ICON_DIR="$TARGET_HOME/.local/share/icons/hicolor/scalable/apps"
    PW_DROPIN="$TARGET_HOME/.config/pipewire/pipewire.conf.d/60-studio-mic-buses.conf"; UNIT="$TARGET_HOME/.config/systemd/user/studiomic.service"
    SHARE_DIR="$TARGET_HOME/.local/share/studio-mic"
fi

echo "======================================================"
echo "  Studio Mic uninstaller ($MODE)"
echo "======================================================"

echo ">> Stopping Studio Mic and removing its PipeWire nodes..."
if [ -x "$BIN_DIR/$BIN_NAME" ]; then
    as_user "$BIN_DIR/$BIN_NAME" --cleanup-pipewire || true
fi
# Belt and braces for older builds without --cleanup-pipewire.
for pid in $(pgrep -u "$TARGET_USER" -x pipewire 2>/dev/null); do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qE 'studiomic-graph\.conf|/studio-mic/sm-'; then kill -TERM "$pid" 2>/dev/null || true; fi
done
pkill -u "$TARGET_USER" -x studiomic 2>/dev/null || true

echo ">> Removing files..."
rm -f "$BIN_DIR/$BIN_NAME" "$BIN_DIR/studio-mic"  # studio-mic: alias from older installs
rm -f "$DESKTOP_DIR/studiomic.desktop" "$TARGET_HOME/.config/autostart/studiomic.desktop"
rm -f "$ICON_DIR/studiomic.svg" "$PW_DROPIN" "$UNIT"
rm -rf "$SHARE_DIR"
rm -f "$TARGET_HOME/.config/environment.d/60-studio-mic-ladspa.conf"
# The Speex wrapper is Studio Mic's own build (nothing else uses it); RNNoise/DeepFilterNet stay.
rm -f "$TARGET_HOME/.local/lib/ladspa/libspeex_ladspa.so"
for dir in "$TARGET_HOME/.local/share/StreamController/plugins" "$TARGET_HOME/.var/app/com.core447.StreamController/data/plugins"; do
    rm -rf "$dir/com.studiomic.controller"
done
rm -rf "$TARGET_HOME/.cache/studio-mic"

command -v update-desktop-database >/dev/null 2>&1 && { update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true; }

echo ""
echo ">> Studio Mic removed. Settings kept in ~/.config/studio-mic/ (and ~/.config/studiomic/ backup)."
echo ">> Remove them too with: rm -rf ~/.config/studio-mic ~/.config/studiomic"
echo ">> Plugins downloaded to ~/.local/lib/ladspa were left in place (other apps may use them)."
echo "======================================================"
