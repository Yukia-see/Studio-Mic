#!/usr/bin/env bash
# StudioMic - Universal Linux Installer
# Professional Broadcast Microphone Tuning & Virtual Mixing Suite
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="studiomic"
APP_TITLE="StudioMic · Material Design 3"

echo "======================================================"
echo "  StudioMic - Universal Linux Installer"
echo "  Professional Broadcast Audio & Virtual Mixing Suite"
echo "======================================================"

# Determine install prefix
INSTALL_MODE="user"
if [ "$1" == "--system" ] || [ "$EUID" -eq 0 ]; then
    INSTALL_MODE="system"
fi

if [ "$INSTALL_MODE" == "system" ]; then
    BIN_DIR="/usr/local/bin"
    DESKTOP_DIR="/usr/share/applications"
    ICON_DIR="/usr/share/icons/hicolor/scalable/apps"
    echo ">> Installing in SYSTEM mode (requires root privileges)"
else
    BIN_DIR="$HOME/.local/bin"
    DESKTOP_DIR="$HOME/.local/share/applications"
    ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
    echo ">> Installing in USER mode for: $USER ($BIN_DIR)"
fi

# 1. Check & Install Required Dependencies
echo ""
echo "[1/5] Checking runtime dependencies..."

check_pkg() {
    command -v "$1" >/dev/null 2>&1
}

if ! check_pkg pipewire; then
    echo "WARNING: PipeWire is not detected. StudioMic requires PipeWire for audio routing."
fi

# Detect Package Manager
PKG_MGR=""
if check_pkg pacman; then
    PKG_MGR="pacman"
elif check_pkg apt-get; then
    PKG_MGR="apt"
elif check_pkg dnf; then
    PKG_MGR="dnf"
elif check_pkg zypper; then
    PKG_MGR="zypper"
fi

echo ">> Detected package manager: ${PKG_MGR:-generic}"

# Prompt to install required LADSPA and GUI packages on Arch/Debian/Fedora if root/sudo available
if [ "$PKG_MGR" == "pacman" ]; then
    MISSING_PACMAN=()
    pacman -Q webkit2gtk-4.1 >/dev/null 2>&1 || MISSING_PACMAN+=("webkit2gtk-4.1")
    pacman -Q lsp-plugins-ladspa >/dev/null 2>&1 || MISSING_PACMAN+=("lsp-plugins-ladspa")
    pacman -Q deepfilternet-ladspa >/dev/null 2>&1 || MISSING_PACMAN+=("deepfilternet-ladspa")
    
    if [ ${#MISSING_PACMAN[@]} -gt 0 ]; then
        echo ">> Notice: The following optional system packages can be installed for LADSPA:"
        echo "   sudo pacman -S --needed ${MISSING_PACMAN[*]}"
        if sudo -n true 2>/dev/null; then
            echo ">> Installing missing audio plugins via pacman..."
            sudo pacman -S --needed --noconfirm "${MISSING_PACMAN[@]}" || true
        else
            echo ">> (Using standalone/bundled LADSPA plugins in ~/.ladspa)"
        fi
    else
        echo ">> All Arch Linux audio packages present (webkit2gtk, lsp-plugins, deepfilternet)."
    fi
fi

# 2. Build or locate release binary
echo ""
echo "[2/5] Building / Locating StudioMic binary..."
cd "$SCRIPT_DIR"

if [ ! -f "target/release/StudioMic-Package" ]; then
    echo ">> Compiling StudioMic release binary with cargo..."
    cargo build --release
fi

if [ ! -f "target/release/StudioMic-Package" ]; then
    echo "ERROR: Failed to find or compile target/release/StudioMic-Package"
    exit 1
fi

# 3. Install Binary
echo ""
echo "[3/5] Installing executable to $BIN_DIR/$BIN_NAME..."
mkdir -p "$BIN_DIR"
# Kill existing instance if running
killall -9 studiomic 2>/dev/null || true
cp -f "target/release/StudioMic-Package" "$BIN_DIR/$BIN_NAME"
chmod +x "$BIN_DIR/$BIN_NAME"

# 4. Install Application Icon & Desktop Launcher (KDE Plasma & GNOME compliant)
echo ""
echo "[4/5] Installing desktop entry and high-resolution icons..."
mkdir -p "$DESKTOP_DIR"
mkdir -p "$ICON_DIR"

# Install Icon
if [ -f "assets/studiomic.svg" ]; then
    cp -f "assets/studiomic.svg" "$ICON_DIR/studiomic.svg"
fi

# Install Desktop Entry
cp -f "studiomic.desktop" "$DESKTOP_DIR/studiomic.desktop"
sed -i "s|^Exec=studiomic|Exec=$BIN_DIR/$BIN_NAME|" "$DESKTOP_DIR/studiomic.desktop"
chmod +x "$DESKTOP_DIR/studiomic.desktop"

# Refresh desktop & icon databases
if check_pkg update-desktop-database; then
    update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
fi
if check_pkg gtk-update-icon-cache; then
    gtk-update-icon-cache -f -t "$(dirname "$ICON_DIR")" 2>/dev/null || true
fi

# 5. LADSPA Plugins Fallback Setup
echo ""
echo "[5/5] Ensuring DeepFilterNet AI & LSP plugins are linked..."
USER_LADSPA_DIR="$HOME/.ladspa"
mkdir -p "$USER_LADSPA_DIR"

if [ -f "/usr/lib/ladspa/libdeep_filter_ladspa.so" ] && [ ! -f "$USER_LADSPA_DIR/libdeep_filter_ladspa.so" ]; then
    ln -sf "/usr/lib/ladspa/libdeep_filter_ladspa.so" "$USER_LADSPA_DIR/"
fi
if [ -f "/usr/lib/ladspa/lsp-plugins-ladspa.so" ] && [ ! -f "$USER_LADSPA_DIR/lsp-plugins-ladspa.so" ]; then
    ln -sf "/usr/lib/ladspa/lsp-plugins-ladspa.so" "$USER_LADSPA_DIR/"
fi

echo ""
echo "======================================================"
echo "  Installation Complete!"
echo "  StudioMic is ready to use."
echo ""
echo "  Run command:        $BIN_NAME"
echo "  Start Minimized:    $BIN_NAME --background"
echo "  Desktop Launcher:   $DESKTOP_DIR/studiomic.desktop"
echo "======================================================"
