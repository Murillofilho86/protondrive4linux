# Maintainer: Murillofilho86 <murillohard@hotmail.com>
#
# Initial local-build PKGBUILD (Fase 1). Not submitted to the AUR yet - see
# ROADMAP.md Fase 5 for the eventual -git + versioned split and AUR submission.
# Builds straight from this fork's `master` branch on GitHub, since no tagged
# release exists yet.
pkgname=protondrive4linux-git
pkgver=0.3.3.r0.g0000000
pkgrel=1
pkgdesc="Bidirectional Proton Drive folder sync for Linux, built on the official proton-drive CLI (unofficial, not affiliated with Proton AG)"
arch=('x86_64')
url="https://github.com/Murillofilho86/protondrive_linux_sync"
license=('MIT')
depends=('gtk3' 'libxkbcommon' 'wayland' 'libx11' 'libxcb' 'mesa' 'xdotool' 'libayatana-appindicator')
makedepends=('rust' 'git')
optdepends=('proton-drive-cli: the official Proton Drive CLI this tool drives (install separately)')
provides=('protondrive4linux')
conflicts=('protondrive4linux')
source=("$pkgname::git+$url.git#branch=master")
sha256sums=('SKIP')

pkgver() {
	cd "$pkgname"
	git describe --long --tags 2>/dev/null |
		sed 's/^v//;s/\([^-]*-g\)/r\1/;s/-/./g' ||
		printf 'r%s.g%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

build() {
	cd "$pkgname"
	cargo build --release --features gui --bins
}

check() {
	cd "$pkgname"
	cargo test --release
}

package() {
	cd "$pkgname"
	install -Dm755 target/release/protondrive4linux "$pkgdir/usr/bin/protondrive4linux"
	install -Dm755 target/release/protondrive4linux-gui "$pkgdir/usr/bin/protondrive4linux-gui"
	install -Dm644 packaging/protondrive4linux-gui.desktop \
		"$pkgdir/usr/share/applications/protondrive4linux-gui.desktop"
	install -Dm644 assets/logo.png \
		"$pkgdir/usr/share/icons/hicolor/256x256/apps/protondrive4linux.png"
	install -Dm644 assets/neutron-logo.svg \
		"$pkgdir/usr/share/icons/hicolor/scalable/apps/protondrive4linux.svg"
	install -Dm644 systemd/protondrive4linux.service \
		"$pkgdir/usr/lib/systemd/user/protondrive4linux.service"
	install -Dm644 systemd/protondrive4linux.timer \
		"$pkgdir/usr/lib/systemd/user/protondrive4linux.timer"
	install -Dm644 systemd/protondrive4linux-watch.service \
		"$pkgdir/usr/lib/systemd/user/protondrive4linux-watch.service"
	install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
	install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
