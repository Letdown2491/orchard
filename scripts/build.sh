#!/usr/bin/env bash
# Builds Orchard inside a container (the host is atomic: no toolchain).
# Builds localhost/orchard-dev-rust from the containerfile on first
# use; rustup and the cargo registry live in named volumes so later
# builds are incremental.
#
#   ./scripts/build.sh check
#   ./scripts/build.sh test --workspace
#   ./scripts/build.sh build --release
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/orchard-dev-rust

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/orchard-dev-rust.containerfile containers/
fi

podman run --rm \
  -v "$PWD":/work:Z -w /work \
  -v orchard-cargo-home:/root/.cargo \
  -v orchard-rustup-home:/root/.rustup \
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.98.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"
