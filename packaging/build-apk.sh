#!/bin/sh
# Build the Alpine apk (musl). Runs INSIDE an alpine:3.22 container,
# never on the host: abuild refuses root, and the deps are alpine's.
#
#   podman run --rm -v "$PWD:/srv:Z" -w /srv alpine:3.22 sh packaging/build-apk.sh <version>
#
# release.yml's apk job calls the same script (its container is this
# image), so CI and local builds share one code path.
#
# Local testing without a pushed tag: abuild skips the fetch when the
# source file already exists in SRCDEST, so a `git archive` tarball
# dropped at /var/cache/distfiles/orchard-<ver>.tar.gz (GitHub layout:
# top-level orchard-<ver>/) builds locally.
#
# NOTES carried from the musl spike (satori-desktop issue #23):
# - alpine 3.22's system cargo (1.87) is below the lockfile's MSRV, so
#   the toolchain is rustup's stable, installed for the builder user.
# - There is no libxkbcommon-x11-dev: the x11 bits ship inside
#   libxkbcommon-dev itself (unlike debian/fedora's split).
# - One build at a time: concurrent containers mounting the same
#   volumes :Z fight over the SELinux labels.
set -eu

VERSION="${1:?usage: build-apk.sh <version> (e.g. 0.5.0)}"
REPO=/srv
WORK=/home/builder/pkg/orchard

# Build-time deps. Runtime deps live in the APKBUILD's depends.
apk add -q abuild build-base sudo git curl pkgconfig fontconfig-dev \
	freetype-dev libxkbcommon-dev wayland-dev vulkan-headers \
	libxcb-dev clang

# abuild refuses root: the builder user owns the build. -r installs
# deps and -a installs the key through sudo, so passwordless sudo for
# the builder user (a build container, not a security boundary here).
adduser -D builder 2>/dev/null || true
addgroup builder abuild 2>/dev/null || true
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
chmod 0440 /etc/sudoers.d/builder
# The source-tarball cache: abuild fetches into SRCDEST (default
# /var/cache/distfiles), group-writable by the abuild group.
mkdir -p /var/cache/distfiles
chown root:abuild /var/cache/distfiles
chmod g+w /var/cache/distfiles
mkdir -p "$WORK" /srv/packaging/apk/out
cp packaging/apk/APKBUILD "$WORK"/APKBUILD
# The version is the caller's truth (CI: the tag; local: the argument),
# not the APKBUILD's memory of the last release.
sed -i "s/^pkgver=.*/pkgver=$VERSION/" "$WORK"/APKBUILD
chown -R builder:builder /home/builder

# The toolchain: rustup's stable for the BUILDER user (the APKBUILD puts
# $HOME/.cargo/bin on PATH; the system cargo's 1.87 is below the lock's
# MSRV: see the spike notes at the top of this script).
su builder -c "curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal"

# Keypair: abuild signs, apk verifies. With APK_SIGN_PRIVATE_KEY set (CI:
# the GitHub secret; generate the pair yourself, `openssl genpkey` or
# abuild-keygen on any machine), the SAME key signs every release, both
# arches: users trust the shipped .pub ONCE and every future version
# verifies clean. Unset (local builds): a throwaway per-build key, and
# users sideload with --allow-untrusted or trust the shipped .pub.
KEYNAME="orchard@orchard"
if [ -n "${APK_SIGN_PRIVATE_KEY:-}" ]; then
	umask 077
	# -o builder: the dir must be READABLE by the builder: a root-owned
	# 0700 .abuild fails closed at abuild-sign (loud, but every release
	# dies; the D84 audit reproduced it in alpine:3.22).
	install -d -o builder -g builder -m 0700 /home/builder/.abuild
	printf '%s\n' "$APK_SIGN_PRIVATE_KEY" > "/home/builder/.abuild/$KEYNAME.rsa"
	# The pub half is DERIVED from the private, so they can never drift.
	openssl rsa -in "/home/builder/.abuild/$KEYNAME.rsa" -pubout \
		> "/home/builder/.abuild/$KEYNAME.rsa.pub" 2>/dev/null
	umask 022
	grep -q "PACKAGER_PRIVKEY=" /home/builder/.abuild/abuild.conf 2>/dev/null || \
		echo "PACKAGER_PRIVKEY=\"/home/builder/.abuild/$KEYNAME.rsa\"" >> /home/builder/.abuild/abuild.conf
else
	su builder -c "abuild-keygen -a -n"
fi
chown -R builder:builder /home/builder/.abuild 2>/dev/null || true
cp /home/builder/.abuild/*.pub /etc/apk/keys/ 2>/dev/null || true
# The copy inherits the 0600 the umask-077 creation gave the source
# (cp derives the mode from the SOURCE masked by umask, not from the
# destination's neighbors): apk index runs as the BUILDER, cannot read
# a root-owned 0600 key, and reports every signature UNTRUSTED (the
# v0.10.0 CI failure on satori-desktop). Alpine's own keys are 0644;
# ours must be too.
chmod 0644 /etc/apk/keys/*.pub 2>/dev/null || true
# The canary: fail HERE, at setup, if the builder cannot read the key:
# not forty lines later inside abuild's signing step.
su builder -c "test -r /home/builder/.abuild/$KEYNAME.rsa.pub"

# abuild fetches the tag tarball from GitHub (network; skipped when the
# tarball already sits in SRCDEST, the local-test path), verifies
# checksums, builds, tests (check(): the core suite), and packages.
su builder -c "cd $WORK && abuild checksum && abuild -r"

# apk filenames carry no arch (that lives in the APK-INFO), so the two
# release jobs' uploads would collide on the release page. Name the
# artifacts like the flatpaks do: version + arch.
ARCH=$(apk --print-arch)
find /home/builder -name '*.apk' 2>/dev/null | while read -r f; do
	cp "$f" "/srv/packaging/apk/out/$(basename "$f" .apk)-$ARCH.apk"
done
cp /home/builder/.abuild/*.pub "/srv/packaging/apk/out/orchard-$VERSION-$ARCH.pub" 2>/dev/null || true
ls -la /srv/packaging/apk/out/
