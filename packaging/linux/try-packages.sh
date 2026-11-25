#!/usr/bin/env bash
# Installs each Linux package of Bowyer in a clean container of its distribution, the way
# the README tells users to, and checks that every library the program links or opens at
# start is there:
#
#   packaging/linux/try-packages.sh target/packages
set -euo pipefail

packages=$(realpath "$1")

check='
  missing=$(ldd /usr/bin/bowyer | grep "not found" || true)
  if [ -n "$missing" ]; then echo "$missing"; exit 1; fi
  for lib in libwayland-client.so.0 libwayland-egl.so.1 libEGL.so.1 libxkbcommon-x11.so.0; do
    ldconfig -p | grep -q "$lib" || { echo "no $lib"; exit 1; }
  done
  test -f /usr/share/applications/bowyer.desktop
  test -f /usr/share/icons/hicolor/256x256/apps/bowyer.png
  echo "ok: $(. /etc/os-release && echo "$PRETTY_NAME")"
'

try() {
  local image=$1 install=$2
  echo "=== $image"
  docker run --rm -v "$packages:/p:ro" "$image" bash -euc "$install > /tmp/install.log 2>&1 || { cat /tmp/install.log; exit 1; }; $check"
}

try ubuntu:22.04 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y /p/bowyer_*_amd64.deb'
try debian:12 'apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y /p/bowyer_*_amd64.deb'
try fedora:latest 'dnf install -y /p/bowyer-*.x86_64.rpm'
try opensuse/tumbleweed 'zypper --non-interactive install --allow-unsigned-rpm /p/bowyer-*.x86_64.rpm'
try archlinux:latest 'pacman -Syu --noconfirm && pacman -U --noconfirm /p/bowyer-*-x86_64.pkg.tar.zst'
