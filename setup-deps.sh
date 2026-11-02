#!/usr/bin/env bash
# Studio Mic - dependency installer
#   * native packages for Arch/CachyOS, Fedora, Debian/Ubuntu (via /etc/os-release)
#   * precompiled LADSPA plugins (DeepFilterNet, RNNoise) into ~/.local/lib/ladspa,
#     pinned by SHA256 - any mismatch aborts immediately
#   * stages the verified RNNoise plugin in assets/plugins/ so the build embeds it; the Speex
#     plugin is built from plugins/speex-ladspa by build.rs (needs the speexdsp headers)
#
# Usage: ./setup-deps.sh [--no-packages] [--stage DIR] [--vendor] [-y]
#   --no-packages  skip the system package manager step
#   --stage DIR    only copy the verified plugins into DIR (package builds); nothing in $HOME is touched
#   --vendor       run `cargo vendor` so packages can be built offline
#   -y             do not ask before using sudo
set -euo pipefail

DF_VERSION="0.5.6"
DF_URL="https://github.com/Rikorose/DeepFilterNet/releases/download/v${DF_VERSION}/libdeep_filter_ladspa-${DF_VERSION}-x86_64-unknown-linux-gnu.so"
DF_SHA256="2ca3205c2911d389604a826a240e745597d50252b5cab81c8248252b335e2236"

RN_VERSION="1.10"
RN_URL="https://github.com/werman/noise-suppression-for-voice/releases/download/v${RN_VERSION}/linux-rnnoise.zip"
RN_ZIP_SHA256="811390b6eb6e28dde023c70590c74d26e74ebb2e595bcf4b95af2341db160e99"
RN_SO_SHA256="ba2b420c76cd2f1e7684430c96fef8c533041d5f67b94060f8eeda6af1701f97"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LADSPA_USER_DIR="${HOME}/.local/lib/ladspa"
DO_PACKAGES=1
STAGE_DIR=""
DO_VENDOR=0
ASSUME_YES=0

while [ $# -gt 0 ]; do
    case "$1" in
        --no-packages) DO_PACKAGES=0 ;;
        --stage) STAGE_DIR="$2"; shift ;;
        --vendor) DO_VENDOR=1 ;;
        -y|--yes) ASSUME_YES=1 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '\033[1;36m>>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

if [ "$(uname -m)" != "x86_64" ]; then
    warn "Precompiled plugins are x86_64 only; install DeepFilterNet/RNNoise LADSPA from your distro instead."
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if sudo -n true 2>/dev/null; then SUDO="sudo"; else SUDO=""; fi
fi

# ------------------------------------------------------------------ 1. native packages
install_packages() {
    [ -r /etc/os-release ] || { warn "No /etc/os-release; skipping system packages"; return 0; }
    # shellcheck disable=SC1091
    . /etc/os-release
    local family="${ID_LIKE:-} ${ID:-}"
    local pm="" required=() optional=()
    case "$family" in
        *arch*|*cachyos*|*manjaro*|*endeavouros*)
            pm="pacman"
            # speexdsp ships its headers on Arch (needed to build the bundled Speex plugin).
            required=(pipewire wireplumber pipewire-pulse ladspa lsp-plugins-ladspa speexdsp webkit2gtk-4.1 gtk3)
            optional=(swh-plugins)
            ;;
        *fedora*|*rhel*|*centos*|*nobara*)
            pm="dnf"
            required=(pipewire wireplumber pipewire-pulseaudio ladspa lsp-plugins-ladspa speexdsp speexdsp-devel webkit2gtk4.1 gtk3)
            optional=(ladspa-swh-plugins)
            ;;
        *debian*|*ubuntu*|*pop*|*mint*)
            pm="apt"
            required=(pipewire wireplumber pipewire-pulse ladspa-sdk lsp-plugins-ladspa libspeexdsp1 libspeexdsp-dev libwebkit2gtk-4.1-0 libgtk-3-0 pulseaudio-utils)
            optional=(swh-plugins)
            ;;
        *)
            warn "Unrecognised distro '${ID:-?}'; install pipewire, wireplumber, ladspa and lsp-plugins yourself."
            return 0
            ;;
    esac
    say "Detected ${PRETTY_NAME:-$ID} (${pm})"
    if [ -z "$SUDO" ] && [ "$(id -u)" -ne 0 ]; then
        warn "sudo not available; skipping system packages: ${required[*]} ${optional[*]}"
        return 0
    fi
    if [ "$ASSUME_YES" -eq 0 ]; then
        read -r -p "Install system packages with ${pm} (uses sudo)? [Y/n] " ans || ans="n"
        case "${ans:-Y}" in [Nn]*) return 0 ;; esac
    fi
    case "$pm" in
        pacman)
            $SUDO pacman -S --needed --noconfirm "${required[@]}"
            for p in "${optional[@]}"; do $SUDO pacman -S --needed --noconfirm "$p" 2>/dev/null || warn "optional package '$p' not in the repos (an AUR helper may have it)"; done
            ;;
        dnf)
            $SUDO dnf install -y "${required[@]}"
            for p in "${optional[@]}"; do $SUDO dnf install -y "$p" 2>/dev/null || warn "optional package '$p' unavailable"; done
            ;;
        apt)
            $SUDO apt-get update -qq
            $SUDO apt-get install -y "${required[@]}"
            for p in "${optional[@]}"; do $SUDO apt-get install -y "$p" 2>/dev/null || warn "optional package '$p' unavailable"; done
            ;;
    esac
}

# ------------------------------------------------------------------ 2. pinned plugin downloads
ladspa_dirs=("$LADSPA_USER_DIR" "$HOME/.ladspa" /usr/lib/ladspa /usr/lib64/ladspa /usr/local/lib/ladspa)
find_plugin() {
    local f
    for d in "${ladspa_dirs[@]}"; do
        f="$d/$1"
        [ -f "$f" ] && { echo "$f"; return 0; }
    done
    return 1
}

verify() { # file expected-sha256
    local got
    got="$(sha256sum "$1" | cut -d' ' -f1)"
    if [ "$got" != "$2" ]; then
        rm -f "$1"
        die "Checksum mismatch for $(basename "$1"): expected $2, got $got. Refusing to install."
    fi
}

fetch() { # url dest
    if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then wget -q -O "$2" "$1"
    else die "Need curl or wget to download plugins"; fi
}

matches_sha256() {
    [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]
}

# Package builds (--stage) must not change the builder's home: no ~/.local/lib/ladspa installs.
user_install() { # file
    [ -n "$STAGE_DIR" ] && return 0
    mkdir -p "$LADSPA_USER_DIR"
    find_plugin "$(basename "$1")" >/dev/null || install -m644 "$1" "$LADSPA_USER_DIR/"
}

install_plugins() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    if [ -n "$STAGE_DIR" ] && matches_sha256 "$STAGE_DIR/libdeep_filter_ladspa.so" "$DF_SHA256"; then
        say "DeepFilterNet LADSPA already present in $STAGE_DIR"
    elif existing="$(find_plugin libdeep_filter_ladspa.so)" && matches_sha256 "$existing" "$DF_SHA256"; then
        say "DeepFilterNet LADSPA found: $existing"
        if [ -n "$STAGE_DIR" ]; then install -Dm644 "$existing" "$STAGE_DIR/libdeep_filter_ladspa.so"; fi
    else
        say "Downloading DeepFilterNet LADSPA ${DF_VERSION}"
        fetch "$DF_URL" "$tmp/libdeep_filter_ladspa.so"
        verify "$tmp/libdeep_filter_ladspa.so" "$DF_SHA256"
        user_install "$tmp/libdeep_filter_ladspa.so"
        if [ -n "$STAGE_DIR" ]; then install -Dm644 "$tmp/libdeep_filter_ladspa.so" "$STAGE_DIR/libdeep_filter_ladspa.so"; fi
    fi

    if [ -n "$STAGE_DIR" ] && matches_sha256 "$STAGE_DIR/librnnoise_ladspa.so" "$RN_SO_SHA256"; then
        say "RNNoise LADSPA already present in $STAGE_DIR"
    elif existing="$(find_plugin librnnoise_ladspa.so)" && matches_sha256 "$existing" "$RN_SO_SHA256"; then
        say "RNNoise LADSPA found: $existing"
        if [ -n "$STAGE_DIR" ]; then install -Dm644 "$existing" "$STAGE_DIR/librnnoise_ladspa.so"; fi
    else
        say "Downloading RNNoise (noise-suppression-for-voice) ${RN_VERSION}"
        fetch "$RN_URL" "$tmp/rnnoise.zip"
        verify "$tmp/rnnoise.zip" "$RN_ZIP_SHA256"
        command -v unzip >/dev/null 2>&1 || die "unzip is required"
        unzip -q -o "$tmp/rnnoise.zip" -d "$tmp/rn"
        local so="$tmp/rn/linux-rnnoise/ladspa/librnnoise_ladspa.so"
        [ -f "$so" ] || die "librnnoise_ladspa.so missing from archive"
        verify "$so" "$RN_SO_SHA256"
        user_install "$so"
        if [ -n "$STAGE_DIR" ]; then install -Dm644 "$so" "$STAGE_DIR/librnnoise_ladspa.so"; fi
    fi

    # Stage RNNoise for embedding into the binary (build.rs re-checks the SHA256).
    for rn in "$STAGE_DIR/librnnoise_ladspa.so" "$(find_plugin librnnoise_ladspa.so || true)"; do
        if [ -n "$rn" ] && matches_sha256 "$rn" "$RN_SO_SHA256"; then
            install -Dm644 "$rn" "$SCRIPT_DIR/assets/plugins/librnnoise_ladspa.so"
            break
        fi
    done

    # LSP from an older manual install in ~/.ladspa keeps working (Studio Mic searches it too).
    if ! find_plugin lsp-plugins-ladspa.so >/dev/null && ! find_plugin lsp-plugins/lsp-plugins-ladspa.so >/dev/null; then
        warn "LSP plugins not found: gate, de-esser, compressor, Clipguard and bus limiters will be bypassed."
        warn "Install 'lsp-plugins-ladspa' (Arch/Fedora/Debian package) for the full chain."
    fi
}

# ------------------------------------------------------------------ 3. LADSPA_PATH for the session
export_ladspa_path() {
    local envd="${XDG_CONFIG_HOME:-$HOME/.config}/environment.d"
    mkdir -p "$envd"
    cat > "$envd/60-studio-mic-ladspa.conf" <<EOF
# Written by Studio Mic setup-deps.sh
LADSPA_PATH=/usr/lib/ladspa:/usr/lib64/ladspa:/usr/local/lib/ladspa:\${HOME}/.local/lib/ladspa:\${HOME}/.ladspa
EOF
    say "LADSPA_PATH exported for new sessions ($envd/60-studio-mic-ladspa.conf)"
}

if [ "$DO_PACKAGES" -eq 1 ]; then install_packages; fi
install_plugins
[ -z "$STAGE_DIR" ] && export_ladspa_path

for bin in pw-cli pw-dump pw-link pw-record pw-play pw-metadata wpctl pactl; do
    command -v "$bin" >/dev/null 2>&1 || warn "missing tool: $bin (install pipewire / pipewire-pulse / wireplumber)"
done

if [ "$DO_VENDOR" -eq 1 ]; then
    say "Vendoring Rust crates for offline builds"
    (cd "$SCRIPT_DIR" && cargo vendor --locked vendor >/dev/null && mkdir -p .cargo && \
        printf '[source.crates-io]\nreplace-with = "vendored-sources"\n\n[source.vendored-sources]\ndirectory = "vendor"\n' > .cargo/config.toml)
fi

say "Dependencies ready."
