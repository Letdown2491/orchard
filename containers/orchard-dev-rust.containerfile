# Orchard's build image (the host ships no compiler, and CI has no
# kuma tree to derive from): Fedora at the version the release
# targets. That is a hard pin, not a rolling one: flatpak apps run
# against their runtime's glibc (freedesktop 25.08 carries 2.42, the
# Fedora 43 set), so a binary built on a newer Fedora dies with
# "GLIBC_2.4x not found" inside the sandbox. 43 is what 25.08 needs;
# the binary still runs on the newer host (glibc is backwards
# compatible). Bump this with the runtime the publish manifest
# targets. Plus gcc for the C glue some -sys crates generate, and the
# gpui native stack: fontconfig and freetype (text), xkbcommon
# (input), clang-devel (for bindgen), and pkg-config so those builds
# find their libraries.
#
#   podman build -t localhost/orchard-dev-rust \
#     -f containers/orchard-dev-rust.containerfile containers/
FROM fedora:43

RUN dnf -y install gcc pkgconf-pkg-config fontconfig-devel freetype-devel \
      libxkbcommon-devel libxkbcommon-x11-devel clang-devel \
      flatpak python3 \
  && dnf clean all
