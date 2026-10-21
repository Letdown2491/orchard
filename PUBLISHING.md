# Publishing to Orchard

Publishing is one command. It takes your flatpak app tree, turns it into a
deterministic OCI artifact, pushes it to your own registry, and signs
Nostr events that tell every Orchard client where the artifact lives and
what it is. Orchard never hosts your bytes: your registry does, your media
URLs do. What Orchard carries is small signed metadata, a few KB per app.

    orchard-publish app.publish.toml --registry ghcr.io/you/your-app

Your registry serves the artifact; the events pin it by digest; the client
verifies one against the other before any install. What you sign is what
installs.

## What you need

1. **An app tree.** The flatpak layout: `files/` (the app), `metadata`
   (the sandbox ini), `export/` (desktop file and AppStream, what
   `flatpak build-export` produces).
2. **An OCI registry you can push to.** ghcr, quay, or self-hosted. The
   artifact is a plain OCI image; nothing about it is Orchard-specific
   beyond its config labels.
3. **A Nostr key.** The publisher identity. Its public key is your name on
   Orchard: it signs every release, reviews attach to it, clients pin it.
4. **The `orchard-publish` CLI.** Not on crates.io: build it from the
   [orchard repo](https://github.com/Letdown2491/orchard) with
   `./scripts/build.sh build --release`. The script needs podman (the
   build runs in a pinned container, so no host toolchain) and drops the
   binary at `target/release/orchard-publish`.

## The app tree and sandbox permissions

The tree's `metadata` ini is the sandbox source of truth: `[Context]`
filesystems and sockets, and `[Session Bus Policy]` entries. It is baked
into the image config labels byte-for-byte at build time.

Sandbox permissions are never published in events. The client reads them
from the artifact itself when the page opens, so what the permissions
viewer shows is exactly what install will grant. There is no
declared-versus-actual gap because there is only one source: the artifact.

The `[Application]` section's `runtime=` line matters beyond the build:
flatpak's image install path does not resolve runtimes from remotes the
way a classic install does, so the client reads this line from the
artifact and pre-flights the deploy, installing the runtime first (user
scope when a remote serves it there, otherwise system scope under
polkit) and chaining the app install behind it. Keep the line accurate;
a runtime no enabled remote serves fails the install up front with that
fact, before image layers download. The tree's `metadata` is what the
client reads; no separate runtime declaration is needed in the publish manifest.

One build constraint before the first push: `files/` runs against the
runtime's glibc, and glibc only goes backwards. Freedesktop 25.08
carries 2.42 (the Fedora 43 set), so a binary built on a newer toolchain
dies inside the sandbox with `GLIBC_2.4x not found`. Build on the Fedora
version the runtime targets, or older; the repo's own build container
pins Fedora 43 for exactly this reason.

## From a flatpak manifest to an app tree

Orchard never runs `flatpak-builder`; the artifact is built from the
*finished* tree. If your app already builds with a flatpak manifest
(`com.example.App.yml`, modules, SDK), one `flatpak-builder` run
produces everything Orchard needs:

    flatpak-builder --force-clean --user --install-deps-from=flathub \
      build-dir com.example.App.yml

That leaves a build directory whose interesting parts are:

- `build-dir/files/`: the installed app (bin, lib, share)
- `build-dir/export/`: the desktop file and AppStream metadata
  `flatpak-builder` extracts from the build; it stays there after
  `build-export`, so nothing needs re-deriving
- `build-dir/metadata`: the sandbox ini (`[Application]`, `[Context]`,
  `[Session Bus Policy]`) the build declared

Assemble Orchard's tree from exactly those three; skip `var/` (builder
scratch, empty but shippable-looking):

    mkdir -p tree
    cp -r build-dir/files tree/files
    cp -r build-dir/export tree/export
    cp build-dir/metadata tree/metadata

`metadata` is the sandbox ini `[Application]` plus `[Context]` and
`[Session Bus Policy]`: what the build declared is what the artifact
carries, and what the client's permissions viewer shows. Check it by hand
once; a module that quietly needed more sandbox than you intended is
visible here before your users find out. Bus access needs the
`[Session Bus Policy]` section (`org.freedesktop.Flatpak=talk` for
`flatpak-spawn --host`): a `talk` key under `[Context]` is silently
ignored, and every spawn then fails with "Portal call failed".

The rest is the ordinary publish: point the manifest's
`[artifact].tree` at that directory and run `orchard-publish`. CI does
the same two steps (build, assemble) before the push-and-sign step;
Orchard's own `self/assemble.sh` is the minimal example (a prebuilt
binary into `files/bin`, everything else committed).

## The manifest

A TOML file next to your tree. Everything the publisher owns that is not
in the artifact itself.

```toml
[app]
id = "com.example.App"                  # the flatpak app id
name = "App"                            # shown in the catalog
summary = "Does a thing"                # one line
description-file = "description.md"     # long form, relative path; or inline `description`
icon = "https://you.example/icon.png"   # URL you host
screenshots = ["https://you.example/shot.png"]   # URLs, best first
categories = ["Utilities"]              # AppStream categories
license = "MIT"                         # optional
repository = "https://github.com/you/app"   # source, shown as a link; a github
                                            # repo can host your proof (--prove)
homepage = "https://you.example"        # optional; omit if the repo is the page

[release]
version = "1.2.0"                       # or ORCHARD_PUBLISH_VERSION in CI
channel = "stable"                      # stable (default) or beta
arch = "linux-x86_64"                   # this build's target
platforms = ["linux-x86_64", "linux-aarch64"]   # full coverage the listing claims
notes = "What changed in 1.2.0."        # the release's changelog

[artifact]
tree = "tree"                           # path to the app tree, relative to this file

[publish]
relays = ["wss://relay.orchard.rocks", "wss://relay.primal.net", "wss://nos.lol"]   # where the events go
tag = "latest"                          # OCI tag for the manifest push
author-name = "Your Name"               # optional; signs a Nostr profile (kind 0)
nip05 = "you@yourdomain.example"        # optional; the verified-address claim
# key-file = "./publisher.key"          # optional; ORCHARD_NSEC beats it
```

Field notes:

- `categories` must use the client's exact chip spellings: Games,
  Utilities, Audio & Video, Development, Network, Office, Graphics,
  Science, Education, System. AppStream's canonical names (`Utility`,
  `AudioVideo`, `Game`) fold to these only for Flathub's metadata; a
  listing event carries categories verbatim, and the client matches by
  exact string. An unrecognized category still lists, but only under All.
- `platforms` is the listing's coverage claim and must name every arch any
  CI leg builds. Publication requires an asset for every declared platform.
- Runtime requirements come from the app tree's metadata.
- `relays` must include `wss://relay.orchard.rocks`, the rendezvous relay
  the project operates, listed first. The third-party relays are
  redundancy and can refuse or drop Orchard events at any time, so a
  publish that never lands on the required relay has no copy on
  infrastructure the project controls. `orchard-publish` refuses such a
  manifest before building anything; `--relays` bypasses the check for
  local tests, and a manifest with no `[publish]` section sends to the
  dev relay.
- The nip05 claim is verified by your domain's
  `/.well-known/nostr.json` naming your key. Until that record exists the
  client shows your profile name or a truncated key; a claim without the
  DNS record never renders as verified.

## What a publish run does

1. **Builds the artifact** deterministically from the tree: sorted layer
   entries, mtime zero, so the same tree always produces the same manifest
   digest. The layer gains a generated appstream snippet
   (`files/share/app-info/xmls/<app-id>.xml.gz`, one release entry with
   this build's version): it is the only thing `flatpak list` reads its
   version column from.
2. **Pushes and reads back.** Monolithic blob upload plus manifest PUT,
   then a fetch of the manifest *by digest*. The signed events only go out
   if the registry serves exactly the bytes that were built.
3. **Signs the events** with your key: manifest, config and layer assets per architecture (kind 3063:
   manifest hash, URL, MIME type, size, platform and format), one release
   (kind 30063: version, channel, notes, references to every asset), a listing
   (kind 32267: name, summary, description, categories, media, platform coverage,
   license, links), and
   a profile (kind 0) when `author-name` or `nip05` is set.
4. **Sends them to the relays** in `[publish].relays`.

Relays are mailboxes, not trust roots: events are signed, so which relay
carried one affects availability only.

### Flags

| Flag | Meaning |
|---|---|
| `--registry host/repo` | push layouts to this registry; without it, publication uses the manifest URLs in prepared metadata |
| `--tag tag` | registry tag prefix override (`[publish].tag`, or `latest`, by default); each image uses `<tag>-<platform>`, while events pin the manifest digest |
| `--relays url,url` | override the manifest's relay list for this run; also bypasses the required-relay check |
| `--nsec <key>` | signing key (hex or nsec); `ORCHARD_NSEC` env also works |
| `--arch linux-aarch64` | build for this architecture; prepared `--asset` inputs already record their architecture |
| `--out dir` | directory for the layout, manifest, bootstrap archive and asset metadata (default `target/publish/<app-id>`) |
| `--legacy-oci` | add a matching legacy registry location to each manifest asset so v0.2.0 clients can update; requires registry manifest URLs |
| `--upload-only` | build or load prepared outputs and push to `--registry`, recording the manifest URL without signing events; permits one architecture of a multi-arch release |
| `--asset file` | load prepared asset metadata; repeat for every declared platform when publishing; local layouts are required only when pushing with `--registry` |
| `--skip-push` | skip registry uploads while still signing; use the registry manifest URL or an already-hosted manifest URL |
| `--skip-relays` | print signed events without contacting relays or inspecting hosted manifests; `--registry` still pushes unless `--skip-push` is set |
| `--prove` | after a successful publish, refresh the repository proof file (below) |

Dry runs: `--skip-relays` keeps test events off the public relays entirely.
Never point a test manifest's relays at the public rendezvous set; test
events are real events once a relay has them.

## Publish to a registry

The build-and-publish command remains:

    orchard-publish app.publish.toml --registry ghcr.io/you/your-app

Publication checks complete platform coverage, verifies the hosted manifest
and config, and signs the assets and one combined release. Registry tags use
`<tag>-<platform>`; the signed URLs use immutable manifest digests.
Credentials remain `ORCHARD_REGISTRY_USER` and `ORCHARD_REGISTRY_PASSWORD`.
`--tag` overrides `[publish].tag`.

For CI jobs that upload one architecture before the others are ready, use
`--upload-only --registry ghcr.io/you/your-app --arch linux-x86_64`. This builds
and pushes without loading a signing key or publishing events, and records the
registry URLs for the manifest and all components in the `.asset.json` file. The final publication job needs
only those metadata files:

    orchard-publish app.publish.toml \
      --asset dist/com.example.App-linux-x86_64.asset.json \
      --asset dist/com.example.App-linux-aarch64.asset.json

Without `--registry`, publication verifies the hosted manifest and config rather
than requiring local layouts or manifest files. It checks the manifest hash,
size and Flatpak app/architecture ref before signing the complete release.

## Events and inspection

A release uses `d=app-id@version`, references its application with `a`, and
references all its assets with `e`. Each architecture has a manifest asset,
a config asset, and one asset per layer (three assets for the current
single-layer build). Every component has `T=flatpak-oci`, its applicable `f`
platform, and `url`, `x`, `size`, `m` and `filename` describing its own bytes.
The manifest MIME is `application/vnd.oci.image.manifest.v1+json`; config and
layers use their descriptor MIME types. Shared components can carry repeated
`f` tags when published once for several platforms.

This implementation proposes an optional **software target** field, `T`, on
NIP-82 software asset events (kind 3063): the intended packaging or execution environment when platform
(`f`) and MIME type (`m`) alone cannot identify a compatible consumer.
`flatpak-oci` identifies components of a Flatpak OCI image. Consumers
match the target together with platform and MIME type; an unknown target must
not be assumed compatible. Lowercase `t` remains a hashtag and does not select
an installation target. The optional `T` convention is proposed for adoption
by other NIP-82 implementers. The [ngit software publishing guide](https://ngit.dev/protocol/software-publishing)
describes release files with installation and dependency resolution left to
clients. Orchard uses that model for config/layer assets; reconstructing a
Flatpak OCI image from them requires a consumer that supports this workflow.

Orchard verifies the manifest and config to display permissions. Layers are
only downloaded for installation, when their hashes are checked against the
manifest. Registry URLs retain the registry authentication flow. Public GHCR
URLs still require an anonymous bearer-token exchange; a plain HTTP downloader
may receive 401. Registry support does not imply every NIP-82 downloader can
fetch those URLs. For consumers without registry support, host the same bytes
at directly downloadable URLs, for example on Blossom. An archive
is useful for bootstrap installation but is not needed to inspect permissions.

## Proving your repository

The listing's `repository` link is otherwise a free claim: nothing stops
a listing for someone else's app from naming someone else's repo. The
proof closes that. Run with `--prove` and, after the publish lands, the
CLI writes an `orchard.txt` to the repository's own default branch
(via the GitHub contents API) containing your publisher key as an npub:

    orchard-publish app.publish.toml --registry ghcr.io/you/your-app --prove

What makes this a proof is where the file lives. Clients derive the URL
from the listing's `repository` tag itself
(`raw.githubusercontent.com/<owner>/<repo>/HEAD/orchard.txt`), so a
listing and its proof sit at the same address: naming a repository
means hosting the file at that repository, and an impersonator must
control the real repo. The `HEAD` path resolves the default branch at
read time, so there is nothing to keep in sync.

Details worth knowing:

- `--prove` needs `GITHUB_TOKEN` in the environment, with `contents:
  write` on the repository. Without the token the step skips with a
  note and the repo keeps whatever proof it has; the publish itself is
  unaffected either way.
- The token route is GitHub-only today. A manifest whose repository
  points at another forge prints a note and proves nothing; the client
  shows such a listing as unverified rather than guessing.
- Until the repository is public, the raw URL does not resolve and the
  client honestly shows unverified.
- In the client, a proven listing wears a check beside the repository
  link on the page and a quiet check on its catalog row. One fetch per
  repository per session; any fetch failure is unverified, never a
  guess.
- The file may carry several keys, one per line (npub or hex), which
  is how a team or a rotated key set keeps one repository proven.

## Multi-arch

One listing, one button in the client, and the device picks the artifact.
The shape:

- Assets are per-arch; a single release with `d=<app-id>@<version>`
  references all of them. Architecture jobs must not sign separate releases
  with that same address, because they would replace each other.
- The listing's platform tags name every architecture the project ships.
- CI runs one upload job per arch (passing `--arch` and `--upload-only`),
  then one publication job signs the combined release and listing.

On an aarch64 machine, an app published only for x86_64 still shows, with
an honest "no aarch64 build yet, published for x86_64" instead of an
install button that cannot work.

## CI workflow

`.github/workflows/self-publish.yml` is a working example (Orchard
publishing itself). The shape to copy, replacing the manifest, assembly script
and `com.example.App` with your application's paths and ID:

```yaml
name: publish

on:
  push:
    tags: ["v*"]

permissions:
  contents: write
  packages: write

jobs:
  prepare:
    strategy:
      fail-fast: false
      matrix:
        include:
          - arch: linux-x86_64
            runner: ubuntu-latest
          - arch: linux-aarch64
            runner: ubuntu-24.04-arm
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - name: Install podman
        run: sudo apt-get update && sudo apt-get install -y podman

      - name: Build
        run: ./scripts/build.sh build --release

      - name: Assemble the app tree
        run: ./your/assemble.sh

      - name: Push artifact and export metadata
        env:
          ORCHARD_REGISTRY_USER: ${{ github.actor }}
          ORCHARD_REGISTRY_PASSWORD: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # Run the Fedora-built publisher in its build environment.
          podman run --rm -v "$PWD:/work:Z" -w /work \
            -e ORCHARD_REGISTRY_USER -e ORCHARD_REGISTRY_PASSWORD \
            -e ORCHARD_PUBLISH_VERSION="${GITHUB_REF_NAME#v}" \
            localhost/orchard-dev-rust \
            target/release/orchard-publish your.publish.toml \
            --upload-only --arch "${{ matrix.arch }}" --out dist \
            --registry "ghcr.io/${GITHUB_REPOSITORY,,}"

      - uses: actions/upload-artifact@v4
        with:
          name: asset-${{ matrix.arch }}
          path: dist/*.asset.json
          if-no-files-found: error

      - uses: actions/upload-artifact@v4
        if: matrix.arch == 'linux-x86_64'
        with:
          name: publisher
          path: target/release/orchard-publish
          if-no-files-found: error

      - name: Attach the artifact to the release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # The digest-pinned docker:// one-liner cannot work against
          # ghcr: flatpak's image fetch never authenticates (it does
          # not even try a token on a 401). The release therefore
          # carries the artifact itself, as an oci-archive the README's
          # two-liner installs with flatpak's local path.
          cp "dist/com.example.App-${{ matrix.arch }}.oci.tgz" \
            "your-app-${{ matrix.arch }}.oci.tgz"
          gh release create "$GITHUB_REF_NAME" --generate-notes || true
          gh release upload "$GITHUB_REF_NAME" \
            "your-app-${{ matrix.arch }}.oci.tgz" --clobber

  publish:
    needs: prepare
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/download-artifact@v4
        with:
          pattern: asset-*
          path: dist
          merge-multiple: true

      - uses: actions/download-artifact@v4
        with:
          name: publisher
          path: dist/bin

      - name: Sign the combined release
        env:
          ORCHARD_NSEC: ${{ secrets.ORCHARD_NSEC }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          sudo apt-get update && sudo apt-get install -y podman
          chmod +x dist/bin/orchard-publish
          podman run --rm -v "$PWD:/work:Z" -w /work \
            -e ORCHARD_NSEC -e GITHUB_TOKEN \
            -e ORCHARD_PUBLISH_VERSION="${GITHUB_REF_NAME#v}" \
            registry.fedoraproject.org/fedora:43 \
            dist/bin/orchard-publish your.publish.toml \
            --asset dist/com.example.App-linux-x86_64.asset.json \
            --asset dist/com.example.App-linux-aarch64.asset.json --legacy-oci --prove
```

Each architecture uploads to GHCR and attaches its bootstrap archive. Only the
asset metadata and publisher executable pass to the final job, which verifies
the hosted manifests and signs one release referencing both architectures.

Notes:

- The only secret you must add is `ORCHARD_NSEC`. On ghcr,
  `GITHUB_TOKEN` (as the password, with your actor as the user) rides the
  bearer-token challenge; other registries use their own credentials.
- `ORCHARD_PUBLISH_VERSION` overrides the manifest's version, so one
  manifest serves every future tag.
- Registry tags default to `latest-<platform>`, keeping matrix legs separate.
  The client installs by the manifest digest the event carries.
- Attach the archive to the release for bootstrap installation: flatpak's
  own docker:// fetch cannot authenticate. Orchard fetches registry artifacts
  through its own authenticated path.
- `contents: write` matters: `--prove` refreshes `orchard.txt` on the
  default branch through the `GITHUB_TOKEN`, and a read-only token
  fails that write after the artifact and events are already out.
- ghcr packages are private by default: flip the package public in its
  settings after the first push, or publication verification and installs
  cannot fetch it. The repository proof also needs a public source repository.
- The workflow fires on `v*` tags only. Pushing the branch publishes
  nothing.

## Local validation

`--skip-relays` prints signed events without contacting relays or inspecting
hosted manifests. It still requires manifest URLs; supplying
`--registry` pushes layouts unless `--skip-push` is set. For a network-free
dry run, use prepared assets with URLs and a disposable signing key,
without `--registry`.
`--relays` overrides the manifest's relay list for local tests.

## Protocol transition

Release addresses change from `app@version@platform` to `app@version`.
Assets use standard manifest URL, MIME, size and bare hash fields, with an
indexable software target. The self-publishing workflow passes `--legacy-oci`
to add a digest-pinned `oci` tag to each manifest asset. v0.2.0 reads that
registry location, while new clients read the standard `url` and component
assets. Both paths install the same image, through one `app@version` release.
Keep this flag enabled while v0.2.0 clients need an update path. It does not
repair the separate v0.1.x installer limitation described in the changelog.

Other publishers can opt into the same transition with `--legacy-oci` when
using registry manifest URLs. Blossom-only metadata cannot supply a legacy
registry location and is rejected with this flag. Component-only publication
without it requires an updated client.

## Your publishing key

The key is the identity: whoever holds it publishes as you, with your
listing, your release history, your reviews. Treat it like the domain
registrar credential it effectively is.

- Generate it once, offline, with Nostr tooling you trust, and put it in
  the repository secret (or a `key-file` outside the repo).
- Never paste it into a chat, an agent session, or a shell history. If a
  key has passed through anything like that, retire it: publish a
  migration to a fresh key (below) and stop using it.
- Without a configured key, publication generates an ephemeral identity and
  prints its secret with a warning. Configure a persistent key for releases.
  Upload-only mode does not load a signing key.

## Rotating keys

Rotation is a statement from your OLD key (kind 30066), not a server
account change. The `migrate` tool ships in the repo
(`crates/orchard-nostr/examples/migrate.rs`; build it with
`./scripts/build.sh build --release --package orchard-nostr --example
migrate`, the binary lands in `target/release/examples/`). The
sequence:

1. Generate the successor key and set it up as the new `ORCHARD_NSEC`.
2. From the old key, sign the migration naming the successor:

       migrate --nsec <old-key> --relay wss://relay.primal.net com.example.App <successor-pubkey-or-npub>

   The relay flag matters: a migration only reaches clients through
   the relays your readers use, and the tool's default relay is the
   local dev one. Repeat per relay if your listing's relays differ.
3. Publish the next release with the new key. Once the successor's own
   listing exists, the old row steps aside and clients show one listing,
   now owned by the new key.

The old key retains exactly one power, and it is deliberate: it can
revoke or reclaim with a newer migration statement (`--revoke`). Nothing
else in the system can be forced to accept it again. Guidance:

- **Keep the old key backed up after rotating.** It is the only key that
  can reclaim the app id if the successor key is ever lost or
  compromised. An unrecoverable successor strands the listing on a row
  nobody can update; the archived old key is the recovery path.
- If the old key itself is compromised, revoke whatever it signed last
  the moment you regain control, and rotate again. While a handover
  stands, a stolen old key's listings do not surface; the revoke is for
  taking that power back on your terms.

## Media

Icons are plain URLs in the listing (the `icon` tag). Host them wherever
you host the rest of your project: your site, your forge, any blob
server. Orchard runs no media service and proxies no bytes; the client
fetches them directly into a local cache.

Icons: a PNG URL the listing's `icon` tag names. The list, the page
header, and the installed app's launcher each resolve icons
independently, so host one image that reads at 40 to 128 pixels; 512
square PNG is the safe choice. (The launcher icon is separate: it rides
inside the artifact's `export/share/icons/hicolor/...`, not the
listing.)

Screenshots: plain URLs in the listing's `screenshot` tags, best
first, one tag row each. Host them like icons (Orchard runs no media
service and proxies no bytes); a landscape PNG reads best, and the
page shows the first six. Publisher apps and reviewed (AppStream)
apps render the same strip.

### PNG, not SVG

The listing icon must be a raster (PNG) URL. Fetched icons land in the
client's disk cache under content-hash names with no file extension,
and an SVG behind such a name does not render. Ship the SVG in your
repository for reuse, point the listing at the PNG.

## A good release, checklist

- [ ] Bump `version` (or let `ORCHARD_PUBLISH_VERSION` set it from the tag).
- [ ] `notes` say what changed; they are the changelog clients render.
- [ ] `platforms` lists every arch this tag actually builds.
- [ ] `metadata` ini matches the sandbox the app really needs; the page
      shows it verbatim.
- [ ] `metadata`'s `[Application] runtime=` names a runtime branch your
      users' remotes can serve (see above): the client pre-flights that
      deploy from this line.
- [ ] Icon is a PNG URL (see Media above) that resolves publicly.
- [ ] Upload each architecture and check registry availability.
- [ ] Dry run with all `--asset` files and `--skip-relays`.
- [ ] Run with `--prove` (and a `GITHUB_TOKEN` with `contents: write`)
      so the repository link verifies in clients.
- [ ] Profile published once (`author-name`, `nip05`) and only re-signed
      when it changes.
- [ ] Tag pushed: `v*` is the publish switch; branch pushes publish nothing.

## After the first publish

Two settings-only steps, both easy to miss:

- **ghcr packages are private by default.** Flip yours public in the
  package's settings, or clients cannot pull the artifact.
- **The proof file only resolves from a public repo.** A private
  repository's raw URL does not serve `orchard.txt`, so the client
  honestly shows unverified until the repo is public.

Then verify from the outside, the way a client would:

    curl https://raw.githubusercontent.com/<you>/<repo>/HEAD/orchard.txt
    curl -s https://<relay>/ | grep 32267   # or any Nostr query tool
