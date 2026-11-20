# Orchard

## Build

The host is atomic: there is no host toolchain. Never run cargo on the
host and never shim one; a host build cannot link here and poisons the
shared target/ tree besides. Build only through the container:

    ./scripts/build.sh check              # whole-workspace typecheck
    ./scripts/build.sh test --workspace
    ./scripts/build.sh build --release

The script creates `localhost/orchard-dev-rust` on first use (Fedora base
plus the gpui native stack, per `containers/`), with rustup and the
cargo registry in named volumes so builds are incremental. The pinned
toolchain lives in `rust-toolchain.toml`.

Run the built binary on the host, not in the container: the GPUI shell
needs the host's Wayland session, and the container has no seat. The host
image lacks libxkbcommon-x11 at runtime (the container only needs it to
link), so `lib/` bundles that one library and `scripts/run.sh` sets
`LD_LIBRARY_PATH` before exec. Never mount the cargo/rustup volumes with
`:Z` for read-only debugging either: any `:Z` mount relabels the volume
and breaks the next unlabeled build (see recovery below).

## Volume labels (learned 2026-10-10)

build.sh mounts the cargo/rustup named volumes WITHOUT :Z, and that is
load-bearing: podman labels fresh volumes `container_file_t:s0` (no
category pair), which every container can read. Mounting one with :Z
relabels it to a single container's category pair, and every later
unlabeled run then fails with "cargo: command not found" (the rustup
shims become unreadable). Recovery from the host:

    chcon -R system_u:object_r:container_file_t:s0 \
      ~/.local/share/containers/storage/volumes/orchard-cargo-home/_data \
      ~/.local/share/containers/storage/volumes/orchard-rustup-home/_data

The project bind mount (`$PWD:/work:Z`) keeps its :Z. Relabeling the
project tree is fine; relabeling the volumes is not.

## The libc pin (learned 2026-10-10)

Cargo.lock pins `libc` at 0.2.189, deliberately: gpui's tree pulls `xattr`
0.2.3 (via `zed-async-tar`), whose `platforms!` macro references
`libc::ENOATTR`; libc removed that constant from its linux targets in
0.2.190 (the linux errno is ENODATA). `nix 0.31` (via `which`, via gpui)
floors libc at ^0.2.186, and 0.2.186 through 0.2.189 still carry ENOATTR,
so 0.2.189 satisfies everyone. A bare `cargo update` bumps libc to 0.2.190+
and xattr stops compiling; re-pin with:

    cargo update -p libc --precise 0.2.189

Revisit when gpui ships a tree without xattr 0.2.x.

## Rules

- Commit before build: the tree that produced a binary is a named commit.
- Commits are the agent's to make; pushes wait for an explicit go.
- No em dashes in documentation or code comments; use commas, colons,
  or parentheses instead. Deliberate owner preference.
- Orchard is independent of kuma and kumaui: never write to or commit
  in those trees from this project, even for a fix that looks like it
  belongs there.

## Local services

The dev relay (`orchard-relay`, podman, `ws://127.0.0.1:8008`) runs the
kind-restricted operator config from `relay/config.toml`: foreign kinds
are rejected at admission, and `limit_scrapers` must stay false or the
client's kind-based subscriptions get refused. The image is built
locally from `containers/orchard-relay.containerfile` (upstream
nostr-rs-relay 0.10.0 with quanta's calibration panic patched out:
upstream pins quanta 0.9.3 via prometheus, and this host's warped TSC
core panics it, which took down the tokio worker answering EVENT
writes, so reads worked and writes hung; see the 2026-10-11 incident).
The config bind-mounts from the repo file, so `relay/config.toml` is
the source of truth and a restart applies its edits. The database
lives in the container layer: it survives `podman restart`, not
`podman rm`.

## Docs

`DESIGN.md` is the architecture record: locked decisions, the Nostr
protocol draft, and the milestone history. `PUBLISHING.md` is the
publisher guide; keep it accurate when the publish path changes. Update
DESIGN.md when a decision changes.

## Releases (learned 2026-11-20)

The self-release sequence is in PUBLISHING.md ("Cutting an Orchard
release (self)"): changelog section first, then the version bump in
the manifest and the workspace together, tag last; the tag freezes the
manifest, so late edits do not publish (0.5.1 exists for exactly
this). Reusable workflow files have authoring rules GitHub enforces
with opaque 0-second failed runs: no `${{ }}` in input/secret
descriptions, no `default:` on secret inputs, and the declared push
trigger whose runs are green no-ops (PUBLISHING.md, "Reusable
workflow file gotchas"). Set CI secrets with `gh secret set ... <
file`, never a web paste: a pasted key failed every sign step of the
first 0.5.0 attempt.
