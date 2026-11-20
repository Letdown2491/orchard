# Running an Orchard relay

Orchard needs no special relay software: it runs on stock
[nostr-rs-relay](https://github.com/scsibug/nostr-rs-relay) with an
operator configuration. This directory is the kit: `config.toml` is the
operator config (written for and verified against nostr-rs-relay
0.10.0), and `allowlist-probe.py` verifies a deployment from the
outside, the way a client would meet it.

## Why kind-restricted

An Orchard client reads exactly six event kinds (the NIP-82 draft kinds
plus profiles). Everything else a general relay accepts is storage and
bandwidth an Orchard client can never use, and admission surface for
junk: anyone can sign garbage asset or review events that pass
signature verification, and a general relay keeps them forever.
Restricting admissions to the Orchard set closes that at the door.

The relay stays permissionless: the allowlist is about *kinds*, not
*keys*. Any key can publish listings, releases, assets, reviews, and
migrations to it; that is the trust model. Events are signed, the relay
is a mailbox, and which relay an event came from affects availability,
never integrity.

## The kind set

| Kind  | Meaning |
|-------|---------|
| 0     | profile (publisher and reviewer names) |
| 3063  | asset (the artifact descriptor: location, digest, size) |
| 30063 | release |
| 30065 | review |
| 30066 | migration (key succession for one app id) |
| 32267 | listing |

## Deploying

1. Run nostr-rs-relay 0.10.0: bare binary, systemd, or a container
   (upstream's README shows the podman form with the config and data
   directory bind-mounted).
2. Copy `config.toml` beside it and edit the `[info]` section: the
   real `wss://` URL, name, description, and the operator contact.
3. The relay reads its config once at startup, from `config.toml` in
   its working directory or a `--config <path>` flag. A malformed
   config exits loudly at startup, which is the safe direction.
   Restarting applies a changed config; there is no hot reload.
4. Terminate TLS in front: nostr-rs-relay speaks plain WebSocket, and
   public relays must be `wss://`. nginx, Caddy, or Cloudflare all
   work; upstream's `docs/reverse-proxy.md` has examples.
5. Mind the database location. `--db` (or the `[database]` section,
   which `--db` overrides) decides where the SQLite files live. The
   default is the relay's working directory; adopting this config onto
   a relay whose database is elsewhere without pinning the location
   makes the restarted relay come up empty.

### The one load-bearing line

`limit_scrapers = false` must stay false. It rejects subscriptions
without author filters, and Orchard clients subscribe by kind. A relay
operator who enables it (upstream's config template recommends it for
the outbox model) refuses every Orchard client REQ. Watch that line
across upstream config-template updates.

### Two behaviors worth knowing before relying on it

- **The allowlist gates admissions, not storage.** Events of other
  kinds already on the relay stay and stay queryable after the config
  change. For a fresh relay that is moot; for an existing general
  relay, old history remains.
- **Pruning exists but is unverified.** The 0.10.0 binary parses a
  `[retention]` section (`max_events`, `max_bytes`, `persist_days`,
  `whitelist_addresses`) that its config template does not document.
  Rehearse it on a dev relay before leaning on it in production.

## Verifying a deployment

From any machine with Python 3 (standard library only):

    python3 relay/allowlist-probe.py wss://your-relay.example.com

Three checks, expected output in order:

1. a validly signed kind 1 is rejected with
   `blocked: event kind is blocked by relay`;
2. a validly signed Orchard migration event is admitted (`OK true`),
   which also proves the probe's signatures are good, so check 1 is
   genuinely the allowlist;
3. a kind-based subscription returns events (on a used relay) or EOSE,
   which proves `limit_scrapers` is false.

The probe exits 0 only when all three pass. Check 2's event carries a
NIP-40 expiration a few minutes out: the relay stops serving it once it
expires and purges it on its own schedule, so probing leaves nothing
behind. Probes run against 0.10.0 or older leave one inert event (a
migration for a throwaway app id clients trim); if that residue must
go, delete the rows from the relay's sqlite directly:

    sqlite3 /path/to/nostr.db "DELETE FROM tag WHERE event_id IN \
      (SELECT id FROM event WHERE kind=30066); \
     DELETE FROM event WHERE kind=30066;"

Also confirm the identity document:

    curl -s -H 'Accept: application/nostr+json' https://your-relay/ | jq .name,.description

## Abuse posture

Solved at admission: foreign-kind junk never lands. Not solved: junk in
the Orchard's own kinds, reviews (kind 30065) chief among them. Any key
can sign reviews, and a sybil can flood them; permissionless is the
design, ranking and disclosure are the client's defenses, and the
relay's first-line friction is `messages_per_sec`. If a specific relay
ever needs per-key policy, nostr-rs-relay has two escalation paths,
both documented in its config template: `[authorization]`
(`pubkey_whitelist`, which ends permissionlessness and is a last
resort) and the `[grpc]` `event_admission_server` (per-event decisions
delegated to an external service over the nauthz protocol), which is
the hook for rate limits or reputation without forking.

## Relaying for the rendezvous set

Nostr relays do not gossip to each other, so an Orchard relay only has
the events published to it. A relay operated for Orchard belongs in the
rendezvous set **in addition to** the third-party relays, not instead
of them: if it is down, new clients still bootstrap through the others.
Publishers' manifests and clients' defaults list all of them, and
discovery is the overlap.

The Orchard relay is the set's required member: every publisher's
manifest must include it, listed first, and `orchard-publish` refuses a
manifest publish that omits it (`--relays` bypasses the check for local
tests). The third-party relays are redundancy, not a guarantee: they
can refuse or drop Orchard events at any time, so the one copy the
project controls is the one every publish must make.

Joining the rendezvous set is a versioned change, so it ships in a tag:
deploy and verify the relay (probe above), then add the `wss://` URL to
`RENDEZVOUS_RELAYS` in `crates/orchard-nostr/src/relay.rs` and to
`relays` in `self/orchard.publish.toml`, commit, and tag again. A relay
deployed after a tag simply misses that tag's events until the next
publish includes it.

## Sizing

Measured on the dev relay (same binary, same config shape): about
27 MB resident memory, 0.03% CPU, and a database in the hundreds of KB.
The relay's cost scales with event count (apps, reviews), not client
count beyond bandwidth, and the database stays small by construction:
even ten thousand apps at three events each is on the order of 100 MB.

| Resource | Floor | Recommended |
|----------|-------|-------------|
| vCPU     | 1     | 2           |
| RAM      | 1 GB  | 2 GB        |
| Disk     | 20 GB SSD | 40 GB SSD |
| Transfer | 1 TB/mo   | 2-5 TB/mo  |

The headroom is for failure modes, not steady-state load:

- **First sync is the bandwidth spike.** A fresh client has no `since`,
  so it pulls every event in the six kinds (10-100 MB with a real
  catalog). Steady-state sessions are KB-scale because they are
  since-gated. Social clients who find the relay through its NIP-11
  document get empty answers for everything foreign and are throttled
  by `subscriptions_per_min`.
- **Reconnect storms.** When the relay restarts, every client's retry
  backoff fires within seconds; the burst is TLS handshakes against
  the proxy, which two cores absorb.
- **Disk-full is the slow failure.** Logs, backups, and retention
  experiments all want headroom; the relay's failure mode is silence,
  and a full disk is one of its causes.

Cheap insurance: 512 MB to 1 GB of swap and `Restart=always` in the
service unit, so an OOM kill self-heals instead of being discovered in
the morning. Bare binary under systemd or a container both work; SSD
over spinning disk for the WAL. Back up `nostr.db` plus its WAL on a
timer, and run `allowlist-probe.py` from a different machine on a
schedule; its exit code is the alert.

### Below the floor: a 0.25 vCPU / 256 MB / 5 GB box

The relay itself is measured at 27 MB resident, so a very small box
works, with adjustments:

- **Swap is mandatory** (512 MB) plus `Restart=always`. There is no
  headroom at this size without it.
- **Shrink the queue knobs** in `[limits]`: `broadcast_buffer` 1024
  (default 16384) and `event_persist_buffer` 512 (default 4096). Deep
  event queues buy nothing at the capped write rate and are the one
  place the relay can balloon memory under burst.
- **Run one process, not two.** Either terminate TLS in front (a CDN
  that proxies `wss://` to the origin over plain ws) so the box runs
  nothing but the relay, or terminate locally with nginx (lighter than
  Caddy's Go runtime) and prefer the bare binary under systemd over a
  container.
- **Disk discipline**: cap the journal, and send backups off-box;
  5 GB has no room for local copies, and local copies are not backups
  anyway.

Move up when steady RSS passes about 70% of RAM, disk passes about
60%, or first syncs visibly slow: those are the signs the ecosystem
outgrew the box.

## Operating notes

- **A sick nostr-rs-relay fails silent.** The known failure mode (seen
  once on the dev relay: a `quanta` panic on startup inside a
  container) is a relay that accepts connections, answers cheap
  queries, and hangs on writes. `orchard-publish`'s relay send has a
  30 second timeout, so publishers get an error instead of a hang, and
  the diagnosis is `podman logs orchard-relay | grep panicked` followed
  by a restart. A periodic health check that round-trips a small event
  through the public endpoint is the cheap watchtower. On hosts whose
  CPU trips the panic (warped TSC cores, metrics-rs/quanta#61),
  `containers/orchard-relay.containerfile` builds 0.10.0 with quanta's
  own fix applied; the dev relay runs it.
- **Backup the SQLite files** (`nostr.db` plus WAL) like any other
  stateful service; the events are the discovery layer.
- **The dev relay** (`orchard-relay` podman container,
  `ws://127.0.0.1:8008`) runs this same config with dev values
  (identities localhost, no rate limits so test loops run free),
  bind-mounted from the repo's `relay/config.toml`: the file is the
  source of truth, and a restart applies its edits. Its database lives
  in the container layer: it survives `podman restart`, not
  `podman rm`.
