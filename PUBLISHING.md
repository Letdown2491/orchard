# Publishing to Orchard

Publishing is one command. It takes your flatpak app tree, turns it into a
deterministic OCI artifact, pushes it to your own registry, and signs four
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

## The app tree and sandbox permissions

The tree's `metadata` ini is the sandbox source of truth: `[Context]`
filesystems, sockets, and `[Session Bus]` policies. It is baked into the
image config labels byte-for-byte at build time.

Sandbox permissions are never published in events. The client reads them
from the artifact itself when the page opens, so what the permissions
viewer shows is exactly what install will grant. There is no
declared-versus-actual gap because there is only one source: the artifact.

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
  `[Session Bus]`) the build declared

Assemble Orchard's tree from exactly those three; skip `var/` (builder
scratch, empty but shippable-looking):

    mkdir -p tree
    cp -r build-dir/files tree/files
    cp -r build-dir/export tree/export
    cp build-dir/metadata tree/metadata

`metadata` is the sandbox ini `[Application]` plus `[Context]` and
`[Session Bus]`: what the build declared is what the artifact carries,
and what the client's permissions viewer shows. Check it by hand once;
a module that quietly needed more sandbox than you intended is visible
here before your users find out.

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
runtime = "org.freedesktop.Platform/x86_64/25.08"   # per-arch segment auto-adjusted
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

- `platforms` is the listing's coverage claim and must name every arch any
  CI leg builds. Each leg publishes an identical listing, so a missing
  arch here is a missing button on someone's machine.
- `runtime` is written once; the arch segment is rewritten per build leg
  (`.../x86_64/25.08` becomes `.../aarch64/25.08` on the arm runner).
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
3. **Signs four events** with your key: an asset (kind 3063: digest, size,
   location, runtime), a release (kind 30063: version, channel, notes,
   reference to the asset), a listing (kind 32267: name, summary,
   description, categories, media, platform coverage, license, links), and
   a profile (kind 0) when `author-name` or `nip05` is set.
4. **Sends them to the relays** in `[publish].relays`.

Relays are mailboxes, not trust roots: events are signed, so which relay
carried one affects availability only.

### Flags

| Flag | Meaning |
|---|---|
| `--registry host/repo` | push target; without it the artifact stays a local OCI layout |
| `--tag tag` | push tag override (the manifest's `[publish] tag` by default). The event's location carries the digest, so this never decides what a client installs; CI pushes `latest-<arch>` so matrix legs don't share a tag |
| `--relays url,url` | override the manifest's relay list for this run; also bypasses the required-relay check |
| `--nsec <key>` | signing key (hex or nsec); `ORCHARD_NSEC` env also works |
| `--arch linux-aarch64` | build and sign for this arch (CI matrix legs) |
| `--out dir` | local layout directory (default `target/publish/<app-id>`) |
| `--skip-push` | build and sign, push nothing |
| `--skip-relays` | sign, send nothing |
| `--prove` | after a successful publish, refresh the repository proof file (below) |

Dry runs: `--skip-relays` keeps test events off the public relays entirely.
Never point a test manifest's relays at the public rendezvous set; test
events are real events once a relay has them.

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

- Releases and assets are per-arch: the release's `d` tag is
  `<app-id>@<version>@<platform>`, so one relay holds both arches of one
  version side by side.
- The listing is per-coverage: its `platforms` tags name every arch the
  project ships, identical from every publish leg.
- CI runs one job per arch (passing `--arch`) and both sign the same
  listing.

On an aarch64 machine, an app published only for x86_64 still shows, with
an honest "no aarch64 build yet, published for x86_64" instead of an
install button that cannot work.

## CI workflow

`.github/workflows/self-publish.yml` is a working example (Orchard
publishing itself). The shape to copy:

```yaml
name: publish
on:
  push:
    tags: ["v*"]            # the tag is the publish switch

permissions:
  contents: write           # the repository proof (--prove)
  packages: write           # ghcr push

jobs:
  publish:
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
      - run: sudo apt-get update && sudo apt-get install -y podman
      - run: ./scripts/build.sh build --release      # or your own build
      - run: ./your/assemble.sh                      # produce the app tree
      - name: Push and sign
        env:
          ORCHARD_NSEC: ${{ secrets.ORCHARD_NSEC }}
          ORCHARD_REGISTRY_USER: ${{ github.actor }}
          ORCHARD_REGISTRY_PASSWORD: ${{ secrets.GITHUB_TOKEN }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}   # the proof write
        run: |
          export ORCHARD_PUBLISH_VERSION="${GITHUB_REF_NAME#v}"   # v1.2.3 -> 1.2.3
          target/release/orchard-publish your.publish.toml \
            --arch "${{ matrix.arch }}" \
            --registry "ghcr.io/${GITHUB_REPOSITORY,,}" \
            --tag "latest-${{ matrix.arch }}" \
            --prove

      - name: Attach the artifact to the release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # flatpak's docker:// image fetch never authenticates, and
          # token registries (ghcr) always challenge, so the release
          # carries the artifact itself: your users' bootstrap is a
          # curl plus `flatpak install --user oci-archive:`
          tar -C "target/publish/your.app.Id" -czf "your-app-${{ matrix.arch }}.oci.tgz" .
          gh release create "$GITHUB_REF_NAME" --generate-notes || true
          gh release upload "$GITHUB_REF_NAME" \
            "your-app-${{ matrix.arch }}.oci.tgz" --clobber
```

Notes:

- The only secret you must add is `ORCHARD_NSEC`. On ghcr,
  `GITHUB_TOKEN` (as the password, with your actor as the user) rides the
  bearer-token challenge; other registries use their own credentials.
- `ORCHARD_PUBLISH_VERSION` overrides the manifest's version, so one
  manifest serves every future tag.
- `--tag latest-<arch>` keeps the matrix legs off one tag. The tag is
  a handle for humans and container tools; the client installs by the
  digest the event carries.
- Attach the artifact to the release (the step above): it is the only
  flatpak-reachable copy, because flatpak's docker:// fetch cannot
  authenticate.
- `contents: write` matters: `--prove` refreshes `orchard.txt` on the
  default branch through the `GITHUB_TOKEN`, and a read-only token
  fails that write after the artifact and events are already out.
- ghcr packages are private by default: flip the package public in its
  settings after the first push, or installs cannot fetch it (and the
  proof file stays unreadable too).
- The workflow fires on `v*` tags only. Pushing the branch publishes
  nothing.

## Your publishing key

The key is the identity: whoever holds it publishes as you, with your
listing, your release history, your reviews. Treat it like the domain
registrar credential it effectively is.

- Generate it once, offline, with Nostr tooling you trust, and put it in
  the repository secret (or a `key-file` outside the repo).
- Never paste it into a chat, an agent session, or a shell history. If a
  key has passed through anything like that, retire it: publish a
  migration to a fresh key (below) and stop using it.
- If `orchard-publish` finds no key it generates an ephemeral one and
  prints it with a loud warning. That identity dies with the run unless
  you capture the nsec immediately. Almost never what you want.

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

Screenshots: the listing does not carry them yet. A publisher app's
page shows no screenshot strip today; reviewed (Flathub) apps get
theirs from AppStream. Plan the `image` tag for a future release.

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
- [ ] Icon is a PNG URL (see Media above) that resolves publicly.
- [ ] Dry run first: `--skip-relays --registry localhost:5000/...` against
      a throwaway registry, then the real run. (Orchard's own dry run:
      the demo registry at `https://localhost:5000/orchard` and the dev
      relay `ws://127.0.0.1:8008`, per DESIGN.md's local loop.)
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
