# Maintainer: StudioMic Project <https://github.com/studiomic/studiomic>
pkgname=studiomic
pkgver=0.1.0
pkgrel=1
pkgdesc="Professional Broadcast Microphone Tuning & Virtual Mixing Suite (Material Design 3)"
arch=('x86_64')
url="https://github.com/studiomic/studiomic"
license=('Apache-2.0' 'MIT')
depends=(
    'pipewire'
    'wireplumber'
    'webkit2gtk-4.1'
    'gtk3'
    'deepfilternet-ladspa'
    'lsp-plugins-ladspa'
)
optdepends=(
    'easyeffects: EasyEffects AI audio integration'
    'obs-studio: Open Broadcaster Software studio routing'
)
makedepends=('rust' 'cargo' 'clang' 'pkg-config')
source=("$pkgname-$pkgver.tar.gz::https://github.com/studiomic/studiomic/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
    cd "$srcdir/$pkgname-$pkgver"
    cargo build --release --locked
}

package() {
    cd "$srcdir/$pkgname-$pkgver"
    install -Dm755 "target/release/StudioMic-Package" "$pkgdir/usr/bin/studiomic"
    install -Dm644 "studiomic.desktop" "$pkgdir/usr/share/applications/studiomic.desktop"
    install -Dm644 "assets/studiomic.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/studiomic.svg"
    install -Dm644 "studiomic.service" "$pkgdir/usr/lib/systemd/user/studiomic.service"
}
