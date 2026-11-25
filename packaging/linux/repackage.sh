#!/usr/bin/env bash
# Builds the .rpm (Fedora, openSUSE) and the pacman package (Arch) of Bowyer from the files
# of its .deb, so the three install the same thing:
#
#   packaging/linux/repackage.sh target/packages/bowyer_<version>_amd64.deb <version>
#
# Needs rpmbuild (apt package `rpm`) and Docker (makepkg runs in an Arch Linux container).
# The packages go next to the .deb.
set -euo pipefail

deb=$(realpath "$1")
version=$2
out=$(dirname "$deb")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work" 2> /dev/null || true' EXIT

# The files of the .deb: usr/bin/bowyer, the desktop entry, the icons, the licenses.
mkdir -p "$work/deb"
(cd "$work/deb" && ar x "$deb")
data=$(ls "$work/deb"/data.tar.*)

# --- rpm ------------------------------------------------------------------------------------
# rpmbuild finds the libraries the program links by itself; the ones it opens at start
# (Wayland, EGL) are asked for by file, which Fedora and openSUSE both resolve.
mkdir -p "$work/rpm/SOURCES" "$work/rpm/SPECS"
cp "$data" "$work/rpm/SOURCES/data.tar"
sed "s/@VERSION@/$version/" "$here/bowyer.spec" > "$work/rpm/SPECS/bowyer.spec"
rpmbuild --define "_topdir $work/rpm" -bb "$work/rpm/SPECS/bowyer.spec"
cp "$work"/rpm/RPMS/x86_64/bowyer-"$version"-1.x86_64.rpm "$out/"

# --- pacman ---------------------------------------------------------------------------------
mkdir -p "$work/arch"
cp "$data" "$work/arch/data.tar"
sed "s/@VERSION@/$version/" "$here/PKGBUILD" > "$work/arch/PKGBUILD"
docker run --rm -v "$work/arch:/build" archlinux:latest bash -euc '
  pacman -Syu --noconfirm --needed base-devel > /dev/null
  useradd -m builder
  chown -R builder /build
  su builder -c "cd /build && makepkg --nodeps --noconfirm --skipinteg"
  chmod -R a+rwX /build
'
cp "$work"/arch/bowyer-"$version"-1-x86_64.pkg.tar.zst "$out/"
ls -la "$out"
