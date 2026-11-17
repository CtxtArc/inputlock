# Build & install from this directory:  makepkg -si
pkgname=inputlock
pkgver=0.1.1
pkgrel=1
pkgdesc="Lock keyboard and mouse on sway while the screen keeps updating; unlock with your password"
arch=('x86_64' 'aarch64')
license=('MIT')
depends=('pam' 'libxkbcommon' 'gcc-libs' 'glibc')
optdepends=('sway: binding-mode switching (required for full lock on sway)')
makedepends=('cargo')
backup=('etc/pam.d/inputlock')
options=('!lto')

prepare() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release
}

check() {
  cd "$startdir"
  cargo test --frozen --release
}

package() {
  cd "$startdir"
  install -Dm755 target/release/inputlock "$pkgdir/usr/bin/inputlock"
  install -Dm644 pam/inputlock            "$pkgdir/etc/pam.d/inputlock"
  install -Dm644 contrib/sway.conf        "$pkgdir/usr/share/inputlock/sway.conf"
  install -Dm644 README.md                "$pkgdir/usr/share/doc/inputlock/README.md"
  install -Dm644 LICENSE                  "$pkgdir/usr/share/licenses/inputlock/LICENSE"
}
