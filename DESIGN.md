# Orchard design

**Status:** draft v0.4 · 2026-10-12

**Pitch:** Orchard is a permissionless Flatpak store. Developers publish apps directly to
their own OCI registries; Nostr carries discovery, identity, releases, and reviews; the
desktop client (Rust + GPUI) renders one unified catalog, including Flathub's, and makes
every app installable as a *native* Flatpak through a local index shim. **The store is a
client, not a gatekeeper.**

## Decisions (locked)

| Area | Decision |
|---|---|
| Artifacts | OCI images on developer-owned registries (ghcr, quay, self-hosted); `.flatpak` bundles as fallback |
| Metadata protocol | Nostr events (Zapstore-style, NIP-82 draft); Orchard is a client/indexer, never the source of truth |
| Product surface | Desktop client only (no server-side store) |
| Flathub | Ingested client-side as a read-only "reviewed" channel, locked on: reviewed installs resolve their runtimes and dependencies from it, so the source cannot be hidden (load re-clamps a pre-lock off) |
| Binaries | Developer-owned registries only; Orchard never stores or proxies bytes |
| Media | Publisher-hosted URLs; no Orchard-run media service |
| Sandbox permissions | Never published in events: flatpak bakes `finish-args` into the artifact, the client reads the real sandbox from it; no declared-vs-actual gap |
| Artifact fetch | The client owns it: verify the manifest by digest, download a local OCI layout, hand flatpak `oci:<dir>` (flatpak's own `docker://` fetch never authenticates; ghcr always challenges) |
| Install scope | User scope for Orchard apps (flatpak's system-scope image install is broken upstream); Flathub installs stay system scope |
| Runtime pre-flight | A publisher install deploys the artifact's own `runtime=` first: already user-deployed, plain install; system-deployed, `--no-deps` (flatpak's `oci:` path never resolves runtimes from remotes, but run time resolves them across installations); deployed nowhere, the runtime installs from the serving remote, user scope first, system scope under polkit |
| Rendezvous relays | A small, public, versioned relay set; every publisher's manifest must include the Orchard-run relay (`wss://relay.orchard.rocks`), enforced by orchard-publish |
| Identity for publishing | NIP-46 remote signer; the client never sees a secret key, the signer's pubkey is the identity |
| Reviews | One per identity per app (kind 30065, parameterized replaceable); editing is republishing, the composer pre-fills an existing review |
| Confirmations | One confirm modal for every consequential action (install, update, remove, job cancel, settings clears); no two-press arming, nothing configurable |
| Update checks | The launch check is unconditional (every session, whatever the setting says); the cadence only picks the repeats: Off, Hourly, Daily (default) |
| Update affordance | No "Update available" badge: the Update button wears the amber itself, on rows and the app page; a publisher-key change is a banner on the page and a line in the confirm modal |
| Update seam | Image updates are uninstall-then-install: flatpak 1.18.4 refuses `flatpak install` over an installed ref at any version (`--or-update` included) and the `oci:` flow's origin remote is transient, so `flatpak update` is blind. The running instance survives the uninstall (deleted deploys stay mapped) and user data is kept. Bundle artifacts (`.flatpak` over https) install with `--bundle` and update in place, no chain |
| Screenshots | The listing carries them as `screenshot` tag rows: absolute URLs, best first, the store renders the first six from the same disk cache the icons use. No media service and no blobs: the publisher hosts the bytes like the icon |
| Language / UI | Rust end-to-end; GPUI `0.2` from crates.io |
| Build | Podman container on the atomic host (see "Build environment") |
| Trust grouping | Flathub listings group by the developer claim (navigation); Orchard listings group by publisher key only |
| Trust scoring | No reputation scores, no WoT-as-a-service: trust renders as facts qualified by the personal graph, never as computed judgments |
| Provenance | Cosign dropped: the Nostr signature binds publisher identity to the pinned digest; a second signature chain adds a trust source without adding trust |

## What each project contributes

**From Flathub:** AppStream-quality metadata, polished catalog UX, one-click native
Flatpak installs, the "verified" badge idea, the shared runtime ecosystem.

**From Zapstore:** publisher-owned release channels, Nostr identity and events,
verify-before-install, community curation via web of trust, lightning/zaps (later).

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│ Orchard client (GPUI, Rust)                             │
│  catalog · search · detail · permissions · install UX   │
└──────┬──────────────────┬──────────────────┬────────────┘
       │ flatpak CLI      │ Nostr relays     │ OCI registry
       │ (orchard-flatpak │ (nostr-sdk)      │ API (OCI
       │  seam, via       │                  │ dist-spec)
       │  flatpak-spawn   │                  │
       │  --host if       │                  │
       │  sandboxed)      │                  │
       ▼                  ▼                  ▼
┌────────────────┐ ┌────────────────┐ ┌────────────────────┐
│ orchard-indexd │ │ publisher      │ │ ghcr / quay / any  │
│ 127.0.0.1:7780 │ │ profiles,      │ │ registry           │
│ Flatpak OCI    │ │ listings,      │ │ (developer-owned   │
│ registry index │ │ releases,      │ │  namespaces)       │
└───────▲────────┘ │ reviews        │ └────────────────────┘
        │          └────────────────┘              ▲
        │ registered as flatpak remote "orchard"   │ publish
        │ oci+http://127.0.0.1:7780                │
        ▼                                          │
   flatpak / GNOME Software / Discover ───────────┘
```

**Key idea: the client is the remote.** `orchard-indexd` runs in-process on `127.0.0.1`
and speaks Flatpak's OCI registry-index protocol, backed by whatever the client learned
from Nostr. Every Orchard app is a normal Flatpak-remote app, `flatpak update` works, and
distribution infrastructure stays at zero central servers.

## Distribution

- Apps are OCI images. Flatpak metadata (ref, permissions, AppStream) rides in OCI
  **config labels** (`config.Labels`); the client reads manifest and config cheaply to
  render the permissions viewer before pulling layers.
- **Image format:** the layer tar is the app tree rooted at `files/`, `metadata`,
  `export/`; flatpak reconstructs the ostree commit at install time from the labels
  (`org.flatpak.ref`, `org.flatpak.metadata`, optional timestamp/subject/body/parent/
  size labels, `org.flatpak.commit-metadata.<key>` carrying base64 GVariant "v" values).
  The commit's `xa.metadata` must match the metadata ini byte-for-byte; the publisher
  supplies it as a commit-metadata label (a string variant serializes as
  `value + "\0\0" + "s"`). The manifest digest is the pinned identity, the same value
  flatpak records as the commit's `Alt-id`.
- **Install path:** verify the manifest by digest against the release event's pin, then
  stream config and layer blobs into a local OCI image layout
  (`~/.cache/orchard/artifacts/<app-id>/<digest>`, content-addressed, staged by rename)
  and hand flatpak `oci:<dir>`. Registry asset URLs identify manifests by digest
  (`https://host/v2/repo/manifests/sha256:...`), so verify and install resolve immutable bytes.
  A finished deploy prunes the app's older layouts; a finished remove drops them all.
- **Bundle assets (Zapstore-shaped):** an asset may instead carry a plain https
  location (`url` tag: blossom and friends), the sha256 pinned on `x`, and the kind
  on `m`/`filename`. The catalog picks only flatpak artifacts (a release's apk and
  AppImage ride the same platform tags, so the kind decides, not just the platform);
  a content-addressed URL path must agree with the digest pin at absorb time. The
  client downloads the bundle and verifies it against the pin: a bundle is one opaque
  file, so the pin is the whole contract and an unpinned bundle refuses to install.
  The bundle header carries the deploy ref and the metadata ini, so the runtime
  pre-flight and the permissions viewer read the same bytes they get from an image
  label. Install is `flatpak install --user --bundle`; flatpak resolves missing
  runtimes from the remotes itself, and bundle installs update in place. The app
  page's digest check never fetches an artifact to decorate the page: a bundle
  verifies against its cached file when one exists, and otherwise the install's
  verify is the first and only download.
- **Bootstrap:** `curl` the release's per-arch oci-archive and
  `flatpak install --user oci-archive:` ; every later update flows through the catalog.
- **Runtime convention:** apps default to runtimes hosted on the flathub remote, so
  runtimes are shared; self-contained images are an explicit opt-in.
- **Publish CLI (`orchard-publish`):** manifest → build → OCI push → sign → Nostr
  events. The layer and gzip are deterministic, so the same tree builds to the same
  digest. Registry pushes speak the Docker v2 bearer-token dance (anonymous or basic
  auth); the proof-file write accepts 201 and 200. CI (`self-publish.yml`) runs on a `v*` tag, builds in the dev image, and
  takes the release version from the tag.

## Nostr protocol (NIP-82 draft, Zapstore-compatible)

Kinds follow the NIP-82 draft ("software applications") plus NIP-01 profiles, keeping
Zapstore publishers readable on Orchard day one:

| Event | Kind | `d` tag | Tags | Content |
|---|---|---|---|---|
| Publisher profile | 0 | (none) | standard NIP-01 | name, nip05, about |
| App listing | 32267 | `<app-id>` | `name`, `summary`, `icon`, `image`, `t` (categories), `f` (platform), `license`, `repository`, `url` | full description |
| Release | 30063 | `<app-id>@<version>` | `a` (application coordinate), `i`, `version`, `f`, `c` (channel), `e` (all asset refs) | changelog |
| Asset | 3063 | (none) | `i`, `x` (component sha256), `version`, `url`, `m`, `size`, `filename`, `f`, `T` (software target) | (none) |
| Review | 30065 | `<app-id>` | `rating` (0.0 to 5.0, one decimal) | review text |
| Publisher migration | 30066 | `<app-id>` | `p` (successor key, hex; omit to revoke) | note |

- `f` values describe OS and architecture (`linux-x86_64`, `linux-aarch64`).
  On kind 3063 assets, `T` identifies the intended packaging or execution
  environment when platform and MIME type are insufficient to select a compatible
  consumer. The optional software-target field is proposed for adoption by
  other NIP-82 implementers. OCI component resolution is client-specific.
  Manifest, config and layer assets all carry `T=flatpak-oci` and their applicable
  `f` platforms. MIME type distinguishes the component roles; only a manifest is
  an installation entry point. Each `url`, bare SHA-256 `x`, and `size` describes
  that component's bytes. Match the verified manifest's config/layer descriptors
  to assets in the same signed release by hash, checking MIME and size as well.
  Registry URLs use `/v2/<repository>/manifests/sha256:<hash>` for manifests and
  `/v2/<repository>/blobs/sha256:<hash>` for config/layers. Blossom URLs may use
  any host; no sibling-URL convention or ncontainer event is required. Runtime
  and permissions come from the verified config, without downloading layers.
  Public registry URLs can require a bearer-token exchange; generic HTTP
  consumers need directly downloadable copies, such as Blossom URLs.
- One release references all architecture assets. `--upload-only` pushes each
  architecture without signing; `--asset` publishes them together without
  the build tree. Self-publication enables `--legacy-oci`, adding the same
  manifest's registry location for v0.2.0 clients. One release serves both
  readers; config and layer assets do not carry legacy install locations.
  Third-party publishers may omit the legacy location, deliberately requiring
  a component-aware client (0.3.0 or later) for OCI installation. v0.2.0 cannot
  install those component-only releases; native Flatpak bundles are unaffected.
  If other publishers do not adopt `T`, it remains an Orchard convention.
  Unknown tags need not prevent other clients from reading compatible assets,
  but MIME and platform alone cannot distinguish Flatpak OCI from other OCI
  targets. Orchard does not infer a Flatpak target for untagged component graphs.
  `--prepare` exports unsigned layouts and component files for other publishers.
- **Migration:** a 30066 signed by the OLD key hands the app id to a successor; once the
  successor's listing exists, the old row stops projecting. Replaceable per (app id,
  old key); revocation is a newer statement with no `p`. Only the listed key can
  authorize its own replacement.
- **Reviews are replaceable per (pubkey, app id):** one identity holds one review per
  app, editing is republishing, a malformed rating is a protocol-level error.
- Media are plain URLs the publisher hosts; Orchard runs no media service.
- Reading: a reader subscribes the catalog kinds (32267, 30063, 3063, 30065, 30066)
  unbounded and profiles (kind 0) in a separate capped REQ (the newest few hundred).
  On the public relays kind 0 is the whole of Nostr; combined, the profile firehose
  never reaches EOSE and drowns the catalog kinds behind it, so no row ever projects
  (the 2026-10-15 Armada miss).
- Rules: relays are dumb mirrors, the client reads several and merges. Curation is
  published as event lists users opt into. The relay list is itself a published event
  plus a hardcoded fallback. Orchard operates a kind-restricted nostr-rs-relay
  (`relay/` is the operator kit, verified against 0.10.0; admission accepts exactly the
  six kinds above); test and probe events self-expire via NIP-40, and the operator
  purge recipe is in `relay/README.md`.

## Trust without review

1. **Verify before install:** the fetched manifest's digest must match the release
   event's pin, otherwise refuse. The page and rows show the check; verdicts cache on
   disk keyed by location plus pin.
2. **Key continuity:** the client privately pins first-seen publisher keys per app id
   (`continuity.json`, never published). A new key for a known app id wears an amber
   "New publisher key" badge; a migration statement naming the key turns it into a
   green "Publisher migrated". An update from a different key than the deployed one
   confirms in amber with the same disclosure.
3. **Repository proof:** `orchard-publish --prove` writes the publisher's npub to the
   repo's default branch (contents API); the client derives the forge raw URL from the
   repository tag itself. Verified listings wear the check beside the repository link
   and a quiet check on rows. github.com today; other forges are a recorded miss.
4. **NIP-05 verification:** a `name@domain` claim is checked against the domain's
   `/.well-known/nostr.json`, one fetch per claim per session; failure is unverified,
   never a guess.
5. **Review hygiene:** publisher self-reviews are flagged and excluded from aggregates;
   keys first seen under a week ago carry a "new key" marker but are never excluded;
   everything else counts once per identity.
6. **Personal graph:** the signed-in identity's kind 3 (read-only, anonymous, social
   relays) defines the circle; indexd crawls reviewers' contact lists for the two-hop
   set (`/v1/graph`). Glyphs and a circle-only aggregate qualify facts; nothing ranks.
7. **Honest permissions:** read from the artifact itself, human-readable grants with
   risk levels. The sandbox is the mitigation for permissionlessness.

## Flathub ingestion

Read AppStream locally from the user's existing flathub remote: zero mirroring, zero
network for the catalog. Rendered as the "Flathub" channel; Orchard-native apps as the
"Orchard" channel. One unified catalog.

Other enabled classic remotes (FlatPark and the like) ingest as extra channels through
the same reader: `flatpak remotes` enumerates (disabled and `no-enumerate` remotes
skipped, same-URL subset remotes deduped), each remote's AppStream parses with the same
code, and the row carries its origin remote so install, metadata, and updates route to
the installation that hosts it. A remote whose AppStream never synced (just added,
never searched) is refreshed client-side with flatpak's own catalog fetch and merged
late; a dead remote times out and stays absent. The same app id on two remotes renders
once, the built-in remote's row winning; per-channel twins are a follow-up. Orchard
itself stays read-only over flatpak state: no `remote-add`, no key handling.

## Client

- **Top bar:** the mark (home: exits every overlay back to the All apps list with search
  and channel reset), the search box centered, the channel dropdown, the account
  control (profile picture when signed in, sign-in plus Settings when not).
- **Categories:** a chip row (All + categories). The chosen chip wears its size
  ("Games · 214") in the same ink; unselected chips stay bare; All wears the catalog
  total. The footer carries the world's line ("N apps · M installed") and the About
  info icon; the About modal keeps version, license, and source link.
- **Detail page:** identity header with Open / Update / Verify-and-install at top right
  (a running job's progress pill takes the slot), a bottom cushion under the last
  section, and Remove as a quiet danger button at the foot. Key changes surface as an
  amber banner. Risk buttons brighten their tint on hover; nothing inverts.
- **Confirm modals:** one shape for install, update, remove, job cancel, and settings
  clears. Enter confirms; Escape or the backdrop dismisses; the card repeats version,
  publisher/proof state, and medium-and-high grants.
- **Jobs:** the status bar's right side carries stage, percent, elapsed; the pill grows
  a thin progress edge; cancel is a two-press arm on the pill (SIGINT, so flatpak
  unwinds). An exit code is not the outcome: a job is believed only when a fresh
  installation list agrees. Notes and errors hold ten seconds, then the bar goes quiet.
- **Reviews:** section collapsed by default (header shows count and average); compose
  is a mode (`r` or the invitation): digits 1 to 5 rate, Enter posts, Escape leaves
  compose before it closes the page. An existing review pre-fills (caret at the end)
  and the invitation reads "Edit your review".
- **Settings:** Sources, Relays (event and social sets, editable, reset to the
  rendezvous set), Updates cadence, Privacy (remote images, cache clears). Settings
  live in `~/.config/orchard/settings.json` behind a handle shared with indexd, so
  edits persist atomically and apply live (relay edits within a second). Precedence:
  environment beats file beats defaults. Trust-relevant behavior is never configurable.

## Crates

| Crate | Role |
|---|---|
| `orchard-core` | domain types, event model, digest + permission logic |
| `orchard-flatpak` | the *only* place the flatpak CLI is invoked (auto `flatpak-spawn --host` when sandboxed) |
| `orchard-indexd` | local shim: Flatpak OCI registry-index on `127.0.0.1` (+ standalone bin); hosts the Nostr read path (relay threads, verified feed, event cache) and serves the merged catalog |
| `orchard-nostr` | NIP-82 events both ways: the verified read feed, the sync relay client, shared event builders |
| `orchard` | GPUI app |
| `orchard-publish` | manifest → build → OCI push → sign → Nostr event |

## Build environment

The host is an atomic Fedora image: no host toolchain, no host cargo. All builds run in
podman (rustup and the registry in named volumes, toolchain pinned by
`rust-toolchain.toml`); run the binary on the host, which owns the Wayland session.

```
./scripts/build.sh check                 # whole-workspace typecheck
./scripts/build.sh test --workspace
./scripts/build.sh build --release
```

The container's Fedora is a hard pin (Fedora 43, glibc 2.42) because the published
flatpak runs against its runtime's glibc and a newer build toolchain dies inside the
sandbox; bump the pin with the runtime the publish manifest targets, and wipe
`target/release` after bumping.

## Status

| Milestone | Scope | State |
|---|---|---|
| M0 | OCI format spike: image format proven against flatpak 1.18.4 and a live registry; client-side digest verification | done 2026-10-10 |
| M1 | Client shell: GPUI window, Flathub catalog from local AppStream, search, detail, install with progress | done |
| M2 | Flatpak seam in one crate; sandbox networking (sandbox-bound port reachable from host) verified | done |
| M3 | Nostr read path: relay connect, kinds fetched and verified, merge into the publisher channel, proven end to end with a hand-rolled test publisher | done |
| M4 | Permissions viewer (built, reads the real sandbox from the artifact); `orchard://` scheme handler | viewer done; handler not built |
| M5 | Publish CLI: manifest → build → push → sign → events, proven live against a TLS registry | done |
| M6 | Reviews, review trust, circle + two-hop graph, self-distribution (Orchard publishes itself through the ordinary path), rendezvous topology | code complete 2026-10-11 |
| M7 | The public loop: tags ship through CI to ghcr, install and update through the GUI | v0.1.0 through v0.1.4 shipped |

**M7 open items:**

- [x] The update handoff test: attempted live with 0.1.2 (installing 0.1.4): verify
      and download succeeded, the flatpak step failed ("is already installed"). Root
      cause and redesign 2026-10-15: the update seam is now uninstall-then-install
      (see Decisions), proven against hello artifacts and a bundle app. The 0.1.x
      to 0.2.0 hop itself stays manual: the old client performs the update and
      fails, so release notes ask for one uninstall + reinstall, the last manual
      hop. 0.2.0 and later self-update through the new chain (bundle artifacts in
      place, image artifacts through the chain).
- [ ] Purge the six inert probe rows from the production relay's database (operator
      action; recipe in `relay/README.md`).

## Risks / open questions

- Multi-publisher display: two keys can list one app id outside a standing handover,
  and the catalog shows both rows. A primary-row-plus-fold policy anchored in signed
  lineage is proposed and undecided. Related gap: an installed app whose publisher
  handed over should re-anchor to the successor's row for updates.
- GPUI on Linux is its youngest backend; API churns. Pin `0.2.x`; watch signatures.
- Review spam / sybil resistance: executed defenses are in Trust without review; the
  residual is accepted: a fresh client with no circle can still be flooded with noise,
  which nothing prevents (intent is unreadable; only permissions are enforceable).
  Relay-side friction (per-key limits, the nauthz admission hook) is operator policy,
  parked until dogfooding shows a need.

**Resolved 2026-10-11:** event kinds reconciled with Zapstore's `zsp` reference
implementation; license settled to MIT-only; OCI index endpoints and annotation keys
confirmed in M0 (ground truth: `common/flatpak-oci-registry.c` in the flatpak repo).

## References

- Zapstore: https://github.com/zapstore/zapstore
- Flathub website monorepo: https://github.com/flathub-infra/website
- Flathub docs: https://github.com/flathub-infra/documentation
- `flatpak-remote(5)`, `oci+https://` extension: https://www.mankier.com/5/flatpak-remote
- Harbor: request for native Flatpak index support: https://github.com/goharbor/harbor/issues/22997
- Pulp: hosting Flatpak OCI images (`/index/dynamic`): https://pulpproject.org/pulp_container/docs/user/guides/manage-flatpak/
- Flatpak 1.17: direct OCI install / sideloading: https://ostechnix.com/flatpak-1-17-0-oci-direct-install-sideloading/
- flatpak-container-tools (flatpak → OCI): https://gitlab.com/flatpak-container/flatpak-container-tools
- gpui: https://crates.io/crates/gpui · https://gpui.rs
