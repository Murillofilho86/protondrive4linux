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
url="https://github.com/Murillofilho86/protondrive4linux"
license=('MIT')
depends=(
	'gtk3' 'libxkbcommon' 'wayland' 'libx11' 'libxcb' 'mesa' 'xdotool' 'libayatana-appindicator'
	# `gio`, for recoverable local deletes (trash.rs falls back to a manual
	# XDG-trash move without it). Only ever pulled in transitively via gtk3
	# before - declared directly since we call it ourselves, not just build
	# against it.
	'glib2'
	# The official Proton Drive CLI this tool drives - useless without it, so
	# it's a hard dependency, not optional. Three AUR packages currently
	# provide it (proton-drive-cli, -bin, -git); depending on the virtual
	# name lets an AUR helper resolve+build+install whichever one the user
	# wants automatically as part of installing this package - no separate
	# manual step.
	'proton-drive-cli'
)
makedepends=('rust' 'git')
provides=('protondrive4linux')
conflicts=('protondrive4linux')
install=protondrive4linux.install
# rusqlite's `bundled` feature compiles SQLite from C source via the `cc`
# crate, which picks up CFLAGS from the environment. makepkg's default
# OPTIONS include `lto`, which appends -flto=auto to CFLAGS for every
# package; the resulting LTO-only object can't be resolved by the final
# rustc-driven link, producing "undefined symbol: sqlite3_*" errors. This
# only opts the C compilation out of Arch's automatic LTO injection - it has
# no effect on this crate's own [profile.release] lto = true (Cargo/rustc
# LTO, a separate, unrelated setting).
options=('!lto')
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
