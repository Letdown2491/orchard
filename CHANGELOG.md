# Changelog

## v0.3.0 (2026-10-21)

- **Releases publish as NIP-82 component assets.** A self-publication now
  signs a component graph: a config asset, a layer asset per architecture,
  and a manifest referencing them, each digest-pinned in its own kind 3063
  event, with the release pointing at the parts. The `T` tag names the
  installable target (`flatpak-oci` beside the other OCI kinds), so a
  release can carry several OCI flavors without them competing for one
  install button. The wire contract is pinned by a golden fixture test.
- **0.2.0 clients still update in place.** Each manifest asset also carries
  a legacy registry location pinned to the same digest as the asset's own
  `x` tag (the publisher enforces the match, the golden test pins the tag
  set), so the unmodified 0.2.0 reader resolves the new release through its
  existing OCI install path: coming from 0.2.0, update in place, nothing to
  do by hand. The recorded trade-off: a third-party publisher adopting the
  component shape without a legacy location is invisible to 0.2.0 clients,
  which is accepted for releases that do not need the old chain.
- **One release per version, not per architecture.** The tag-driven
  publish previously let per-arch jobs overwrite one release; each runner
  now uploads its artifact and exports unsigned asset metadata, and one
  final job verifies the hosted manifests and signs a single release
  referencing both architectures.
- **Runtime and permissions move into the verified image config.** They
  rode unsigned event tags; they now come from the image config bytes,
  which are hash-pinned in the signed events, so what the pre-flight and
  the permissions viewer show is the same bytes the runtime will run.
- **Install gating keys off the signed shape.** Bundle versus image
  installs are chosen from the release's component assets, a mismatched
  `flatpak_ref` hard-fails the install, and the remaining unreachable
  states in the fetch and install paths return errors instead of panics.
- **From 0.2.0 this update also brings the screenshots feature** (the
  listing `screenshot` tags of v0.2.1, which was never published), so app
  pages gain the screenshot strip beside this release's changes.

## v0.2.1 (2026-10-16)

- **Listings carry screenshots.** The publisher manifest's `[app]`
  gains `screenshots` (absolute URLs, best first), which ride the
  listing event as `screenshot` tag rows; the store parses them into
  the catalog and publisher apps now render the same screenshot strip
  (with the same disk cache) that reviewed AppStream apps always had.
  Older publishers and older stores ignore the tags on both sides:
  a pre-screenshots publish is silent about them, a pre-screenshots
  store just shows no strip.

## v0.2.0 (2026-10-15)

- **Zapstore releases install and update.** Apps published in Zapstore's
  shape now flow through the whole store: the artifact location is a
  plain https URL (a blossom, the bytes named by their own sha256), the
  pin rides the event's digest tag, and the kind rides content-type and
  file-name tags. The catalog picks the flatpak artifact out of a
  release that also ships apk, exe, and AppImage builds (they share the
  platform tags, so the kind, not just the platform, decides what is
  installable); the client downloads the bundle over https and verifies
  it against the pinned sha256 before anything installs; the bundle
  header carries the metadata ini, so the runtime pre-flight and the
  permissions viewer read the same bytes they get from an image label;
  the install is `flatpak install --user --bundle`. A content-addressed
  URL whose path names a different digest than the event's pin is
  rejected outright, and an unpinned bundle refuses to install: the pin
  is the whole contract when the artifact is one opaque file. The
  Orchard channel accepts Zapstore's `main` channel name beside
  `stable`, and a listing built only for other platforms still shows
  (with no install offer), as before.
- **Updates uninstall first, then install.** Flatpak 1.18.4 refuses
  `flatpak install` over an installed ref at any version (`--or-update`
  included), and `flatpak update` is blind here: the oci: install
  flow's origin remote is transient, so the deployment's Origin names a
  remote that does not exist and there is nothing to serve the update
  from. That is what failed the 0.1.2 to 0.1.4 self-update: verify and
  download succeeded, the flatpak step did not. Image updates now run
  as one chained job, uninstall then install: the running instance
  survives the uninstall (deleted deploys stay mapped), user data is
  kept, and a failed install leaves the cached layout one press away.
  The 0.1.2 and 0.1.4 clients updating to 0.2.0 still run the old
  code, which performs the update and hits the old failure: uninstall
  and reinstall by hand once. That is the last manual hop; 0.2.0 and
  later self-update through the new chain.
- **Bundle updates skip the chain.** A bundle install over an installed
  ref updates in place (proven bundle-over-bundle), so a bundle
  artifact's update is a plain install job: no uninstall step.
- **Update jobs say Update.** The publisher update path ran under
  Install labels end to end (Verifying, then Installing, then
  "Install"): an update keeps Update labels and verbs throughout.
- **The Sources section is now Catalog.** Both channels are required
  (Flathub feeds reviewed runtimes, the Orchard channel feeds publisher
  installs and the app's own updates), so neither belongs behind a
  toggle: the section holds only "Opens on". A settings file saved
  before the change (with a source turned off) is re-clamped to on at
  load.
- **The artifact cache prunes itself.** A finished deploy keeps only
  the newest cached artifact per app (each is roughly a hundred
  megabytes and every older one was there for a retry that never
  comes); a finished remove drops all of them.
- **The relay sync splits profiles from the catalog.** One combined
  subscription made first contact with the public relays a treadmill:
  kind 0 is the whole of Nostr, so the profile firehose streamed
  forever, drowned the catalog kinds behind it, and its EOSE never
  came (the 2026-10-15 Armada miss: the events were on the relay, the
  sync never reached them). The catalog kinds now ride their own
  subscription, whose corpus is small enough to finish, and profiles
  ride a capped one (the newest few hundred, plenty for publisher
  names and icons). The backfill anchor waits for both streams'
  EOSE.
- **Reconnects back off and ping.** Relays and the proxies in front
  of them drop long-lived connections without a closing handshake, so
  the session now pings at each read checkpoint to keep the path
  warm, and repeated failures stretch the retry (doubled each miss,
  five seconds to five minutes) until a session that completes a
  backfill or lives a minute resets the stretch: a relay that resets
  once an hour retries in seconds; one that is down is retried every
  few minutes, not hammered and not spamming the log.
- **Bundle pages stop failing their digest check.** The app page's
  digest check ran the registry verifier on every artifact, and a
  https bundle is not a registry artifact (its pin rides the bytes
  themselves), so a Zapstore app's page claimed the artifact did not
  pass its digest check when nothing had failed. Bundles verify
  against their cached file when one exists; before an install, the
  page shows its neutral sandbox note and the install's verify does
  the checking.
- **The sync path stops paying for nothing.** Re-delivered events
  (every reconnect re-pulls the overlap window) are recognized by
  their id and skip re-verification and re-storage; the event cache
  serializes once per burst instead of once per three seconds while
  a backfill streams; a cached bundle hashes through a stream reader
  instead of landing in RAM; and the client's loopback reads share
  one connection pool.

## v0.1.4 (2026-10-15)

- **Flathub is a required source.** Reviewed installs resolve their
  runtimes and dependencies from Flathub, so the settings row no longer
  offers a switch: it wears the same "required" badge as the pinned
  relay, and a settings file saved before the lock (with the source
  turned off) is re-clamped to on at load.
- **Update checks: launch always, daily by default.** The startup
  comparison is not a cadence anymore: it always runs once per launch,
  whatever the setting says, so a user who never closes the app still
  gets a check every session. The cadence picks the repeats (Off,
  Hourly, Daily) and defaults to Daily; the "On launch" option is
  retired, and a file saved with it maps to Daily at load. Users who
  deliberately picked Off or Hourly keep their choice.
- **The Update button carries the signal.** The amber "Update
  available" badge is gone from the catalog rows and the app page: the
  Update button itself now wears the badge's amber everywhere, so the
  row reads one fact shorter. A publisher-key change is still named:
  as the banner on the app page, and now in the confirm modal when the
  update is launched from a row.
- **The settings page scrolls to its end.** The scroll container's
  content child was a flex item in a column flex container, so when the
  page ran longer than the body the child was compressed to fit instead
  of overflowing: gpui measured zero scrollable range, nothing scrolled,
  and the tail ("Clear cached events") painted under the status bar.
  The container now nests the way the app page does: a flex column
  wrapper reserves the body, the scroll div lays its content out at
  natural height, and a bottom cushion keeps the last row clear of the
  footer.
- **Switching channels leaves the app page.** The channel dropdown set
  the filter behind whatever covered the list: the detail page (or a
  collection page, or settings) stayed on screen until "All apps" was
  pressed. A channel click now closes those, handing the list back
  clean the way the back button does.

- **Publisher installs make way for the runtime they need.** Flatpak's
  `oci:` install path does not resolve runtimes from remotes the way a
  classic install does: it accepts only what the installing scope already
  has deployed, so a publisher artifact targeting a runtime branch the
  machine never deployed died with "requires the runtime
  org.freedesktop.Platform/x86_64/26.08 which was not found" (Amethyst,
  issue-free repro on a machine whose runtimes are all system-scope
  installs). The client now reads the `runtime=` the artifact's own
  metadata declares and plans around it: deployed user-scope, the plain
  install proceeds; deployed only system-scope, the app installs
  `--no-deps` and runs against it (a user install runs a system runtime:
  how Orchard itself is deployed); deployed nowhere, the runtime installs
  first, from a user remote when one serves it (no polkit), from the
  system scope under polkit when only a system remote does, and the app
  install chains behind it. A runtime no enabled remote serves fails the
  install up front with that fact, before any download.

## v0.1.3 (2026-10-14)

- **Other enabled flatpak remotes ingest as channels, not just Flathub** (issue #3).
  The catalog rendered exactly two sources, Flathub's local AppStream and the Orchard
  channel, so a second classic remote stayed invisible however deliberately it was
  added. `flatpak remotes` now enumerates at catalog load: disabled remotes stay out
  (the listing never passes `--show-disabled`), `no-enumerate` remotes stay out (the
  flag exists so stores will not list those contents; the origin remotes that
  `.flatpakref` installs create carry it), a flathub subset remote dedupes against its
  parent's URL, and user and system rows of one remote name collapse to a single
  channel. Each remote's AppStream parses through the same reader that serves flathub.
  Rows carry their origin remote: the badge wears the remote's display title, and
  install, remote-info, and update route to the installation that hosts it (a
  user-scope remote takes `--user`; the hardwired flathub install could never have
  served one). The channel dropdown lists what was found. The same app id on two
  remotes renders once, the built-in remote's row winning; per-channel twins with a
  provenance group are the follow-up.

- **A remote whose catalog never synced arrives without a restart.** `remote-add`
  does not download the AppStream branch; flatpak's next `update` or `search` does,
  and only when its cache is a day stale. The loader treated the missing file as a
  logged skip, so a remote added that day (measured live with a user-scope flatpark
  remote: 166 apps in the catalog, none in the store) never appeared at all. The store
  now renders what is already synced first, then fetches the missing catalogs with
  flatpak's own metadata refresh (`update --appstream`; only catalog metadata lands in
  flatpak's cache, nothing installed changes) and merges each channel as it lands. A
  dead remote (the issue's Launcher.moe, DNS gone since 2024) times out inside flatpak
  and simply stays absent; the rest of the store is already up.

- **Installs from the flatpak-distributed app work again.** The self tree's sandbox
  carried the host-flatpak permission as a `talk` key under `[Context]`, a section
  flatpak does not read it from, so the installed app's sandbox never held the
  `org.freedesktop.Flatpak` bus policy and every install, on every channel, died with
  "Portal call failed: ServiceUnknown" the moment it spawned the host flatpak.
  Verified by A/B on the installed app, same binary: the spawn that fails as published
  prints the host's `flatpak --version` once `--talk-name=org.freedesktop.Flatpak` is
  granted. The policy now lives in `[Session Bus Policy]`, the section the client's
  own permissions parser reads, so the viewer shows the grant too. PUBLISHING.md names
  the section correctly and records the failure mode for the next hand-written tree.

## v0.1.2 (2026-10-12)

- **indexd is the read path, not a demo convenience.** indexd started only when a
  catalog file parsed, and the only candidates were dev-side demo files, so a real
  user had no server at all: the Orchard channel, the reviews, the profiles, and the
  two-hop graph all strand on it, since the relay supervisor and crawlers live inside
  its serve. The server now starts unconditionally with an empty base outside a
  checkout, and the wait-for-bind moved inside start so the first fetches cannot
  outrun it. The bind itself rides out a five-second retry: a restart raced the
  previous instance's dying listener for port 7780 and the new session ran indexless,
  its fallback file serving a stale seed.

- **Consequential actions confirm in a modal, and the action lives in the header.**
  The two-press Sure? died on its own detail: a Danger button's hover state flipped
  the fill to the solid risk color exactly under the cursor, so the armed label could
  vanish when confirmation mattered. One modal now fronts every consequential action
  (install, update, remove, job cancel, settings clears), carrying the version, the
  publisher and repository facts, and the medium-and-high sandbox grants, so a
  row-level install reviews the same facts the page shows.

- **The mark goes home; About moves to the footer.** The chrome's most prominent
  click was its least common action. The mark now exits every overlay back to the All
  apps list with search and channel reset; About keeps its modal as a footer info
  icon, ahead of the apps count, where fine print lives.

- **The view's count rides the mark, and the category chip wears it.** The
  composition strip above the rows duplicated the footer's "N apps" and went away; the
  row count sits next to the mark as its home label, live under search, chips, and
  lenses. The chosen category chip carries its count in one ink, All included.

- **Reviewing your reviewed app is an edit, not a blind rewrite.** A replaceable
  review edits by republishing, but the composer met that with an empty draft and the
  old text gone. The draft now pre-fills stars and text, from the page or from the
  sign-in pairing flow, and the invitation reads "Edit your review" once yours is on
  the app.

- **The flatpak-shaped client sees the store, and says what it is.** The sandbox
  granted home but not `/var/lib/flatpak`, where the flathub appstream lives: the
  installed app showed only Orchard apps while the host build loaded thousands from
  the same file. The read-only grant joined the metadata, the account menu's version
  line reads the workspace version so artifact and self-report move together, and the
  repository-proof check speaks on hover like the other badges always did.

## v0.1.1 (2026-10-12)

- **The client owns the registry fetch, and installs from a downloaded layout.** Two
  failures in one real install. Verify fetched registries with plain GETs, but ghcr
  always challenges anonymous pulls (401 plus WWW-Authenticate Bearer), so the digest
  check died before it started: the fetch path now runs the token challenge (realm,
  service, scope, an anonymous token scoped to the repository, retried once and reused
  across manifest, index, and config fetches), proven live against ghcr. And
  flatpak's `docker://` image path cannot authenticate at all, so installs stopped
  handing flatpak a registry reference and instead download the OCI layout locally,
  verify the manifest against the pinned digest, and hand flatpak `oci:<dir>`: the
  bytes the client checked are the bytes that install.

- **Locations carry digests, and the CI legs stop stepping on each other.** Asset
  events publish locations in digest form (`docker://repo@sha256:...`), so any client
  verifies and installs the exact bytes the publish run built; the tag stops deciding
  what a client installs. The push pins a per-arch tag (`latest-<arch>`) so the build
  matrix stops overwriting one handle, the artifact attaches to the GitHub release as
  an oci-archive (the bootstrap path, since flatpak's `docker://` fetch cannot
  authenticate), and the proof-file write accepts 201 beside 200, because the first
  arch to finish creates the file and the second updates it.

- **The published app launches: the container pin moves back to the runtime's
  glibc.** The v0.1.0 artifact died at launch demanding GLIBC_2.43 while the
  freedesktop 25.08 runtime it runs against carries 2.42; the containerfile's comment
  said Fedora at the version the release targets, and the pin had drifted. Now 43.
  The pin bumps with the runtime the publish manifest targets.

## v0.1.0 (2026-10-11)

- **A permissionless Flatpak store, first public release.** The client is a GPUI
  desktop app over one unified catalog: Flathub ingested client-side as a read-only
  reviewed channel (a hand-rolled streaming AppStream parser; no mirroring, no
  network for the catalog), publisher apps from Nostr events verified and merged into
  the Orchard channel. Search, categories, keyboard-first list, full-page details
  with screenshots and releases, update detection.

- **The artifact path is the protocol.** OCI images on developer-owned registries;
  the client verifies the manifest against the signed digest before any install, and
  the sandbox permissions are read from the artifact itself (flatpak bakes
  `finish-args` into the image, so the permissions viewer shows exactly what install
  will grant; no declared-versus-actual gap).

- **The Nostr read path runs end to end.** NIP-82-style events (profile, listing,
  release) fetched from relays, verified against the publisher key, merged into the
  channel live: indexd, the in-process server, is where relay events become catalog
  rows and where the social reads (profiles, follows, the two-hop reviewer graph)
  crawl.

- **Publishing is one command.** `orchard-publish` takes the app tree, builds a
  deterministic OCI artifact, pushes it to the publisher's registry, and signs the
  events that tell clients where it lives; PUBLISHING.md documents the flatpak-builder
  path it rides.

- **Trust is the reader's, not the curator's.** Reviews are real Nostr events (one
  per identity per app), the digest shield marks client-verified pins, and the
  developer claim stays navigation, not identity.

- **Orchard distributes itself through the ordinary path.** The rendezvous relay
  (`wss://relay.orchard.rocks`) is pinned into every publish, a kind-restricted relay
  operator kit ships in `relay/`, tags ship through CI to ghcr, and the install and
  update loop runs through the GUI; the bootstrap is a flatpak one-liner by digest.
  Publisher-channel installs run user scope: flatpak's system-scope image install is
  broken upstream, and user scope needs no polkit dialog.
