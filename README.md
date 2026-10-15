# Orchard

A permissionless Flatpak store. Flathub's UX and artifact format, a
Nostr-based trust model in place of a review gate: publishers ship
directly, clients verify signatures and digests, and the store is a
client, not a gatekeeper.

<p align="center">
  <img src="docs/orchard-main.png" width="49%" alt="The Orchard catalog: Flathub rows beside the Orchard channel, category chips with counts, and the footer total">
  <img src="docs/orchard-details.png" width="49%" alt="A detail page: identity header with Verify and install, the digest pin, the repository check, and sandbox permissions with risk levels">
</p>

## How it works

- **Publishers** push their app as an OCI image to any registry they
  control and sign small Nostr events (listing, release, asset, profile)
  that describe it. `--prove` also drops a proof file into the linked
  source repository, so the repo link is a claim clients can check.
  `PUBLISHING.md` is the guide, including a ready CI workflow.
- **The client** merges those events into a catalog beside Flathub's
  (read from your local AppStream), shows the sandbox permissions the
  artifact really carries, pins the release to its manifest digest, and
  installs through flatpak. Installs are refused when bytes do not match
  the pin. Proven repositories wear a check; the chosen category chip
  wears its count, and the footer carries the store's total.
- **Reviews are Nostr events too**, signed through your own signer over
  NIP-46 Nostr Connect: the client never holds your identity key. One
  review per identity per app (replaceable: editing is republishing, and
  the composer pre-fills yours), publishers cannot review themselves into
  a rating, and a publisher key change is disclosed, with a signed
  handover shown as an authorized migration. Signed in, the review list
  opens on your circle: the people you follow first, everyone one click
  away.
- **Nothing happens without a yes.** Install, update, and remove confirm
  through a modal that repeats the release version, the publisher's
  standing, and the permissions that matter; the job then reports
  progress in the footer, and cancel interrupts flatpak cleanly.
- **No central server.** Discovery is a rendezvous set of public relays;
  binaries live on publisher registries; the client embeds a local index
  shim so every Orchard app is a normal Flatpak-remote app, visible to
  GNOME Software and Discover.
- **Settings apply live.** Channel visibility, relay sets, update
  cadence, and privacy toggles live on a settings page (the account
  menu; a gear on the toolbar when signed out); edits persist to
  `~/.config/orchard/settings.json` and take effect immediately, no
  restart.

## Status

Early but released. Orchard publishes itself, and every release ships
through this repo's own workflow: tags land on the releases page as
signed OCI artifacts, and the installed client updates through Orchard's
own catalog. The full loop works end to end on real infrastructure:
publish, discover, verify, install, update, review. Multi-arch (x86_64
and aarch64) builds in CI. Not built yet: the `orchard://` scheme
handler and signed-lineage folding for multi-publisher listings.
`DESIGN.md` is the architecture record; `PUBLISHING.md` is the publisher
guide.

## Install

Any Linux desktop with flatpak; the client itself is a native flatpak
app (x86_64, aarch64), installed per-user. The release is two commands
away, pinned to the exact bytes it signed:

    curl -LO https://github.com/Letdown2491/orchard/releases/latest/download/orchard-linux-x86_64.oci.tgz
    flatpak install --user -y oci-archive:orchard-linux-x86_64.oci.tgz

(On an aarch64 machine, use `orchard-linux-aarch64.oci.tgz`; the two
arches are separate artifacts.) The archive is the same artifact the
release signed, so the bytes are the pin. Not a `docker://ghcr.io`
one-liner because that cannot work: flatpak's image fetch never
authenticates, and ghcr always challenges (see DESIGN.md); the Orchard
client sidesteps this by downloading the verified artifact itself and
installing from the local layout. The install is per-user (flatpak's
system-scope image install is broken; see DESIGN.md), so no polkit
password appears. Updates after that first install come through
Orchard's own catalog, not the command line.

## Try it (Fedora atomic host, x86_64)

The host has no toolchain; everything builds in a container via
`scripts/build.sh`.

    ./scripts/build.sh check
    ./scripts/build.sh build --release
    ./scripts/run.sh

(Run through `run.sh`, not the binary directly: it sets the library
path the GPUI shell needs and pins the dev relay.)

Out of the box the client browses and installs the real Flathub catalog.
The Orchard channel lists the real releases from the rendezvous relays,
Orchard itself among them; see `PUBLISHING.md` to become a publisher.
(By default `run.sh` pins the demo relay on this machine; run it as
`ORCHARD_RELAYS= ./scripts/run.sh` to exercise the shipped public relay
set instead.)

## Self-publishing

Orchard publishes itself. `self/orchard.publish.toml` is the manifest,
`self/tree/` is the app tree (its `assemble.sh` drops the built binary
into `files/bin`; the metadata, desktop file, and launcher icon are
committed beside it), and `self-publish.yml` is the workflow that ties
it together. Architecture jobs push to GHCR and export asset metadata; one job
publishes a combined release. The version tag triggers it.


**Never run orchard-publish on `self/orchard.publish.toml` bare**: the
manifest's relay list is the public rendezvous set, so a bare run is a
real publish. The local dry run must override both:

    ORCHARD_NSEC=<key> ./target/release/orchard-publish self/orchard.publish.toml \
      --registry https://localhost:5000/orchard --relays ws://127.0.0.1:8008

## Layout

- `crates/orchard-core`: domain model, AppStream parser, OCI verifier,
  permissions translator.
- `crates/orchard-flatpak`: the single seam to the flatpak CLI.
- `crates/orchard-indexd`: the local catalog service (embedded in the
  client), relay sessions, event cache.
- `crates/orchard-nostr`: event model, builders, and the verified feed.
- `crates/orchard`: the GPUI client.
- `crates/orchard-publish`: the publisher CLI (`PUBLISHING.md`).
- `demo/`: a demo app tree and manifest for the local fixture.
- `self/`: Orchard publishing itself (manifest, app tree, assemble
  script), and its CI workflow.
- `relay/`: the operator kit for running a kind-restricted Orchard
  relay (config verified against nostr-rs-relay 0.10.0, verification
  probe, deployment notes).
- `docs/`: screenshots.
- `assets/`: the Orchard icon (listing and launcher rasters).

## Docs

- `PUBLISHING.md`: the publisher guide. The manifest, the CLI, multi-arch,
  the CI workflow, and key rotation.
- `DESIGN.md`: the architecture record. Locked decisions, the Nostr event
  draft, milestones.
- `AGENTS.md`: container build rules and the sharp edges of building on
  an atomic host.

## License

MIT, see `LICENSE`.
