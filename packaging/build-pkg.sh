#!/bin/bash
# Build the Arch package. Runs INSIDE an archlinux container, never on
# the host: makepkg refuses root, and the deps are pacman's.
#
#   podman run --rm -v "$PWD:/srv:Z" -w /srv archlinux:base-devel sh packaging/build-pkg.sh <version>
#
# release.yml's arch job calls the same script, so CI and local builds
# share one code path.
#
# Local testing without a pushed tag: makepkg skips the fetch when the
# source file already exists in the source dir, so a `git archive`
# tarball at /srv/orchard-<ver>.tar.gz (GitHub layout: top-level
# orchard-<ver>/) builds locally. The sha256 is computed from the file
# at hand either way, so the local and CI paths verify the same way.
#
# One build at a time: concurrent containers mounting the same volumes
# :Z fight over the SELinux labels.
set -eu

VERSION="${1:?usage: build-pkg.sh <version> (e.g. 0.5.0)}"
REPO=/srv
WORK=/home/builder/pkg/orchard
TARBALL="orchard-$VERSION.tar.gz"

# Build-time deps. Runtime deps live in the PKGBUILD's depends. -Syu is
# required: arch containers ship a keyring that drifts.
pacman -Syu --noconfirm --needed base-devel rust clang pkgconf git

# makepkg refuses root: the builder user owns the build (a build
# container, not a security boundary here).
useradd -m builder 2>/dev/null || true
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
chmod 0440 /etc/sudoers.d/builder

mkdir -p "$WORK" /srv/packaging/arch/out
cp packaging/PKGBUILD "$WORK"/PKGBUILD
# The version is the caller's truth (CI: the tag; local: the argument),
# and the checksum is computed from the tarball actually present (the
# fetched tag tarball in CI, the local git archive in testing): SKIP
# only as the pre-compute placeholder.
if [ -f "/srv/$TARBALL" ]; then
    cp "/srv/$TARBALL" "$WORK/$TARBALL"
    SUM=$(sha256sum "$WORK/$TARBALL" | cut -d' ' -f1)
elif [ -f "$HOME/.cache/orchard-dist/$TARBALL" ]; then
    cp "$HOME/.cache/orchard-dist/$TARBALL" "$WORK/$TARBALL"
    SUM=$(sha256sum "$WORK/$TARBALL" | cut -d' ' -f1)
else
    SUM=SKIP
fi
sed -i "s/^pkgver=.*/pkgver=$VERSION/" "$WORK"/PKGBUILD
sed -i "s/^sha256sums=.*/sha256sums=('$SUM')/" "$WORK"/PKGBUILD
chown -R builder:builder /home/builder

# build(), check(), package() as the builder. -f: the version sed means
# the source may have been built before; -s: install missing deps.
su builder -c "cd $WORK && makepkg -sf --noconfirm"

mv "$WORK"/orchard-*.pkg.tar.zst /srv/packaging/arch/out/
ls -la /srv/packaging/arch/out/
