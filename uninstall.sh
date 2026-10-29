#!/usr/bin/env bash
# StudioMic - Uninstaller Script
set -e

BIN_NAME="studiomic"

echo "======================================================"
echo "  StudioMic - Uninstaller"
echo "======================================================"

# Determine mode
if [ "$1" == "--system" ] || [ "$EUID" -eq 0 ]; then
    BIN_DIR="/usr/local/bin"
    DESKTOP_DIR="/usr/share/applications"
    ICON_DIR="/usr/share/icons/hicolor/scalable/apps"
else
    BIN_DIR="$HOME/.local/bin"
    DESKTOP_DIR="$HOME/.local/share/applications"
    ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
fi

echo ">> Terminating any active StudioMic instances..."
killall -9 studiomic StudioMic-Package 2>/dev/null || true

echo ">> Removing binary..."
rm -f "$BIN_DIR/$BIN_NAME"

echo ">> Removing desktop launcher..."
rm -f "$DESKTOP_DIR/studiomic.desktop"
rm -f "$HOME/.config/autostart/studiomic.desktop"

echo ">> Removing application icon..."
rm -f "$ICON_DIR/studiomic.svg"

# Refresh desktop & icon databases
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
fi

echo ""
echo ">> StudioMic binary, icons, and autostart entries have been removed."
echo ">> Note: Your user presets in ~/.config/studiomic/ have been preserved."
echo ">> To remove configuration as well, run: rm -rf ~/.config/studiomic"
echo "======================================================"
