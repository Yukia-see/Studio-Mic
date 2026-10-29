# Maintainer: StudioMic Project <https://github.com/studiomic/studiomic>
pkgname=studiomic
pkgver=0.1.0
pkgrel=1
pkgdesc="Native Linux microphone tuning and multi-source virtual mixing suite for PipeWire & WirePlumber"
arch=('x86_64')
url="https://github.com/studiomic/studiomic"
license=('Apache-2.0' 'MIT')
depends=('pipewire' 'wireplumber' 'gtk4' 'libpipewire')
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
    install -Dm644 "studiomic.service" "$pkgdir/usr/lib/systemd/user/studiomic.service"
}
