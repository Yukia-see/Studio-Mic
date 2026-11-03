#!/usr/bin/env bash
# Studio Mic - installer
#
#   ./install.sh            user install  (~/.local/bin, ~/.config/pipewire/pipewire.conf.d)
#   sudo ./install.sh --system   system install (/usr/local), built as $SUDO_USER, never as root
#   ./install.sh --package  build a native package (Arch: makepkg -> pacman -U, Fedora: rpmbuild -> dnf)
#   ./install.sh -y         assume "yes" to the dependency prompt
#
# Safe for existing installs: settings in ~/.config/studiomic are migrated (not deleted) on first
# launch, and the old engine is stopped cleanly before the new binary is copied in.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="studiomic"
MODE="user"
ASSUME_YES=0
for a in "$@"; do
    case "$a" in
        --system) MODE="system" ;;
        --package) MODE="package" ;;
        -y|--yes) ASSUME_YES=1 ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "Unknown option: $a" >&2; exit 2 ;;
    esac
done

say()  { printf '\033[1;36m>>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ root safety
# Never build or touch a user's home as root. Under sudo we drop back to the invoking user.
BUILD_USER="$(id -un)"
if [ "$(id -u)" -eq 0 ]; then
    [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] || die "Run as a normal user (or via sudo from one), not as root directly."
    BUILD_USER="$SUDO_USER"
    [ "$MODE" = "user" ] && MODE="system"
fi
TARGET_HOME="$(getent passwd "$BUILD_USER" | cut -d: -f6)"
as_user() { if [ "$(id -u)" -eq 0 ]; then runuser -u "$BUILD_USER" -- "$@"; else "$@"; fi; }
as_root() { if [ "$(id -u)" -eq 0 ]; then "$@"; elif command -v sudo >/dev/null; then sudo "$@"; else die "need root for: $*"; fi; }

echo "======================================================"
echo "  Studio Mic installer  ·  mode: $MODE  ·  user: $BUILD_USER"
echo "======================================================"

# ------------------------------------------------------------------ 1. dependencies
say "[1/6] Checking PipeWire tools"
missing=()
for bin in pw-cli pw-dump pw-mon pw-loopback pw-play pw-record pw-link pw-metadata wpctl pactl; do
    command -v "$bin" >/dev/null 2>&1 || missing+=("$bin")
done
[ ${#missing[@]} -gt 0 ] && warn "Missing tools: ${missing[*]}"

run_deps="n"
if [ "$ASSUME_YES" -eq 1 ]; then
    run_deps="y"
elif [ -t 0 ]; then
    read -r -p "Would you like to download and install required PipeWire and Noise Suppression dependencies for Studio Mic (DeepFilterNet, RNNoise, Speex)? [Y/n] " run_deps || run_deps="n"
    run_deps="${run_deps:-y}"
fi
case "$run_deps" in
    [Yy]*)
        chmod +x "$SCRIPT_DIR/setup-deps.sh"
        as_user env HOME="$TARGET_HOME" "$SCRIPT_DIR/setup-deps.sh" $([ "$ASSUME_YES" -eq 1 ] && echo -y)
        ;;
    *) say "Skipping dependency setup (run ./setup-deps.sh any time)." ;;
esac

# ------------------------------------------------------------------ 2. native package path
if [ "$MODE" = "package" ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    # Every package build gets the next build number (studio-mic-<version>-<N>). It is only
    # recorded once the build succeeds, and the package is kept in dist/<distro>/.
    ver="$(grep -m1 '^version' "$SCRIPT_DIR/Cargo.toml" | cut -d'"' -f2)"
    num_file="$SCRIPT_DIR/packaging/BUILD_NUMBER"
    build=$(( $(cat "$num_file" 2>/dev/null || echo 0) + 1 ))
    say "Package version $ver, build $build"
    keep_build() { # distro-dir package-file
        printf '%s\n' "$build" | as_user tee "$num_file" >/dev/null
        as_user mkdir -p "$SCRIPT_DIR/dist/$1"
        as_user cp "$2" "$SCRIPT_DIR/dist/$1/"
        say "Saved $(basename "$2") in dist/$1/"
    }
    case "${ID_LIKE:-} ${ID:-}" in
        *arch*|*cachyos*|*manjaro*|*endeavouros*)
            say "[2/6] Building Arch package with makepkg (as $BUILD_USER)"
            work="$(as_user mktemp -d)"
            as_user cp "$SCRIPT_DIR/packaging/arch/PKGBUILD" "$SCRIPT_DIR/packaging/arch/studio-mic.install" "$work/"
            as_user bash -c "cd '$work' && STUDIOMIC_SRC='$SCRIPT_DIR' STUDIOMIC_BUILD='$build' makepkg -f --noconfirm"
            # Only the main package (never a studio-mic-debug-* split package).
            pkg="$work/studio-mic-$ver-$build-x86_64.pkg.tar.zst"
            [ -f "$pkg" ] || die "makepkg did not produce $pkg"
            keep_build arch "$pkg"
            as_root pacman -U --noconfirm "$pkg"
            ;;
        *fedora*|*rhel*|*nobara*)
            say "[2/6] Building RPM with rpmbuild (as $BUILD_USER)"
            top="$(as_user mktemp -d)"
            as_user mkdir -p "$top"/{SOURCES,SPECS}
            as_user tar --exclude=./target --exclude=./.git --exclude=./dist --exclude=__pycache__ -czf "$top/SOURCES/studio-mic-$ver.tar.gz" \
                --transform "s,^,studio-mic-$ver/," -C "$SCRIPT_DIR" .
            as_user rpmbuild --define "_topdir $top" --define "app_version $ver" --define "build_number $build" \
                -bb "$SCRIPT_DIR/packaging/fedora/studio-mic.spec"
            rpm_file="$(ls "$top"/RPMS/*/studio-mic-"$ver"-"$build".*.rpm 2>/dev/null | head -1)"
            [ -n "$rpm_file" ] || die "rpmbuild did not produce a studio-mic-$ver-$build RPM"
            keep_build fedora "$rpm_file"
            as_root dnf install -y "$rpm_file"
            ;;
        *) die "--package supports Arch/CachyOS and Fedora; use the default user install elsewhere." ;;
    esac
    say "Package installed. Launch 'studiomic' (or run 'studiomic --status')."
    exit 0
fi

# ------------------------------------------------------------------ 3. build (always as the user)
say "[2/6] Building release binary (as $BUILD_USER)"
command -v cargo >/dev/null 2>&1 || as_user bash -lc 'command -v cargo' >/dev/null || die "Rust/cargo is required (pacman -S rust)"
as_user bash -lc "cd '$SCRIPT_DIR' && cargo build --release"
BIN_SRC="$SCRIPT_DIR/target/release/StudioMic-Package"
[ -x "$BIN_SRC" ] || die "build failed: $BIN_SRC not found"

if [ "$MODE" = "system" ]; then
    BIN_DIR="/usr/local/bin"; DESKTOP_DIR="/usr/share/applications"
    ICON_DIR="/usr/share/icons/hicolor/scalable/apps"; PW_DROPIN_DIR="/usr/share/pipewire/pipewire.conf.d"
    UNIT_DIR="/usr/lib/systemd/user"; SHARE_DIR="/usr/local/share/studio-mic"
    inst() { as_root install "$@"; }
else
    BIN_DIR="$TARGET_HOME/.local/bin"; DESKTOP_DIR="$TARGET_HOME/.local/share/applications"
    ICON_DIR="$TARGET_HOME/.local/share/icons/hicolor/scalable/apps"; PW_DROPIN_DIR="$TARGET_HOME/.config/pipewire/pipewire.conf.d"
    UNIT_DIR="$TARGET_HOME/.config/systemd/user"; SHARE_DIR="$TARGET_HOME/.local/share/studio-mic"
    inst() { as_user install "$@"; }
fi

# ------------------------------------------------------------------ 4. stop the running engine
say "[3/6] Stopping any running Studio Mic (state is flushed first)"
if command -v "$BIN_DIR/$BIN_NAME" >/dev/null 2>&1; then
    as_user "$BIN_DIR/$BIN_NAME" --quit >/dev/null 2>&1 || true
fi
# Older builds had no control socket: stop them and their engine by PID.
for pid in $(pgrep -u "$BUILD_USER" -x studiomic 2>/dev/null) $(pgrep -u "$BUILD_USER" -x StudioMic-Packa 2>/dev/null); do
    kill -TERM "$pid" 2>/dev/null || true
done
sleep 0.5
for pid in $(pgrep -u "$BUILD_USER" -x pipewire 2>/dev/null); do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qE 'studiomic-graph\.conf|/studio-mic/sm-'; then kill -TERM "$pid" 2>/dev/null || true; fi
done
for pid in $(pgrep -u "$BUILD_USER" -x pw-record 2>/dev/null); do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qE 'rate 8000|StudioMic_'; then kill -TERM "$pid" 2>/dev/null || true; fi
done

# ------------------------------------------------------------------ 5. install files
say "[4/6] Installing to $BIN_DIR"
inst -Dm755 "$BIN_SRC" "$BIN_DIR/$BIN_NAME"
# One command only: drop the old `studio-mic` alias left by earlier installs (only if it is our symlink).
if [ -L "$BIN_DIR/studio-mic" ] && [ "$(readlink "$BIN_DIR/studio-mic")" = "$BIN_NAME" ]; then
    if [ "$MODE" = "system" ]; then as_root rm -f "$BIN_DIR/studio-mic"; else as_user rm -f "$BIN_DIR/studio-mic"; fi
fi
inst -Dm644 "$SCRIPT_DIR/assets/studiomic.svg" "$ICON_DIR/studiomic.svg"
tmp_desktop="$(mktemp)"
sed "s|^Exec=studiomic|Exec=$BIN_DIR/$BIN_NAME|" "$SCRIPT_DIR/studiomic.desktop" > "$tmp_desktop"
inst -Dm644 "$tmp_desktop" "$DESKTOP_DIR/studiomic.desktop"
rm -f "$tmp_desktop"
inst -Dm644 "$SCRIPT_DIR/pipewire-conf/60-studio-mic-buses.conf" "$PW_DROPIN_DIR/60-studio-mic-buses.conf"
tmp_unit="$(mktemp)"
sed "s|^ExecStart=.*|ExecStart=$BIN_DIR/$BIN_NAME --daemon|" "$SCRIPT_DIR/studiomic.service" > "$tmp_unit"
inst -Dm644 "$tmp_unit" "$UNIT_DIR/studiomic.service"
rm -f "$tmp_unit"
if [ "$MODE" = "system" ]; then as_root mkdir -p "$SHARE_DIR"; as_root cp -r "$SCRIPT_DIR/streamcontroller-plugin" "$SHARE_DIR/"
else as_user mkdir -p "$SHARE_DIR"; as_user cp -r "$SCRIPT_DIR/streamcontroller-plugin" "$SHARE_DIR/"; fi
if [ "$MODE" = "system" ]; then as_root find "$SHARE_DIR/streamcontroller-plugin" -name __pycache__ -type d -prune -exec rm -rf {} +
else find "$SHARE_DIR/streamcontroller-plugin" -name __pycache__ -type d -prune -exec rm -rf {} +; fi

command -v update-desktop-database >/dev/null 2>&1 && { update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true; }
command -v gtk-update-icon-cache >/dev/null 2>&1 && { gtk-update-icon-cache -f -t "$(dirname "$(dirname "$ICON_DIR")")" 2>/dev/null || true; }

# ------------------------------------------------------------------ 6. StreamController plugin
say "[5/6] StreamController integration"
PLUGIN_SRC="$SHARE_DIR/streamcontroller-plugin/com.studiomic.controller"
linked=0
for dir in "$TARGET_HOME/.local/share/StreamController/plugins" "$TARGET_HOME/.var/app/com.core447.StreamController/data/plugins"; do
    if [ -d "$(dirname "$dir")" ]; then
        as_user mkdir -p "$dir"
        if [[ "$dir" == *".var/app"* ]]; then
            # Flatpak sandboxes cannot follow symlinks outside their data dir: copy instead.
            as_user rm -rf "$dir/com.studiomic.controller"
            as_user cp -r "$PLUGIN_SRC" "$dir/com.studiomic.controller"
        else
            as_user ln -sfn "$PLUGIN_SRC" "$dir/com.studiomic.controller"
        fi
        say "  plugin installed in $dir"
        linked=1
    fi
done
[ "$linked" -eq 0 ] && say "  StreamController not found (plugin kept in $PLUGIN_SRC)"

# ------------------------------------------------------------------ done
say "[6/6] Done"
cat <<EOF

  Studio Mic is installed.

    Open:            $BIN_NAME
    Tray only:       $BIN_NAME --background
    Control / CLI:   studiomic --status         (studiomic --help for all commands)

  The four buses (Voice Out, Stream Mix, VOD Mix, Monitor Mix) are created as soon as Studio Mic
  starts. They come up at login without Studio Mic after your next login or
  'systemctl --user restart pipewire wireplumber' (this briefly interrupts audio, so it is not
  done automatically).
EOF
