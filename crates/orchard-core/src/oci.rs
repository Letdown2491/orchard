//! Minimal OCI artifact access for verification and install.
//!
//! Orchard pins artifacts by manifest digest. Before any install the client
//! fetches the manifest from the artifact's location, hashes it, and refuses
//! to proceed when the hash differs from the catalog pin. This module speaks
//! just enough of the container ecosystem to do that:
//!
//! - `docker://<registry>/<repo>:<tag>`: the Docker Registry v2 HTTP API
//!   (`/v2/<repo>/manifests/<ref>`, `/v2/<repo>/blobs/<digest>`), which is
//!   what flatpak's own OCI support uses under the hood. Anonymous pulls
//!   work everywhere we ship to: no-auth registries directly, and
//!   token-authenticated ones (ghcr, Docker Hub, quay) through the
//!   standard challenge flow (a 401 with `WWW-Authenticate: Bearer`
//!   names a token endpoint; an anonymous token fetch and retry).
//! - `oci:<dir>`: a local OCI image layout (what `skopeo copy ... oci:` and
//!   podman push produce), which is also flatpak's zero-setup install path.
//!
//! Format notes, validated against flatpak 1.18 sources and a live Fedora
//! registry image: image config labels (capital-L `Labels`) carry
//! `org.flatpak.ref` (required by flatpak), `org.flatpak.metadata` (the app
//! ini), and optional sizes; a manifest may be a per-arch manifest or an
//! image index with a `platform` per entry.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

/// Accept header for manifest fetches: OCI and Docker manifest/index types.
pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// What a successful verification learned about an artifact.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Manifest digest actually seen, `sha256:<hex>`.
    pub digest: String,
    /// Bytes of the (possibly per-arch selected) manifest.
    pub manifest_size: u64,
    /// `org.flatpak.ref` from the image config labels, if present.
    pub flatpak_ref: Option<String>,
    /// `org.flatpak.metadata` from the image config labels, if present.
    pub metadata: Option<String>,
    /// `org.flatpak.installed-size` label, bytes, if present.
    pub installed_size: Option<u64>,
    /// Sum of layer descriptor sizes, bytes.
    pub download_size: u64,
    /// Digest of the image config blob (which carries the labels).
    pub config_digest: Option<String>,
}

/// Hex sha256 of bytes, `sha256:`-prefixed like OCI digests.
pub fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Where an installable artifact lives, parsed from a location string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local OCI image layout directory, e.g. `oci:/path/to/layout`.
    OciDir(PathBuf),
    /// A registry exposed via the Docker v2 API, e.g.
    /// `docker://ghcr.io/owner/repo:tag` or the digest form
    /// `docker://ghcr.io/owner/repo@sha256:<hex>` (tag optional; the
    /// digest pin also rides alongside in the catalog). The registry
    /// part may carry an explicit scheme (`docker://https://…`):
    /// otherwise localhost registries use http and everything else
    /// https.
    Registry {
        registry: String,
        repository: String,
        tag: Option<String>,
        digest: Option<String>,
        scheme: Option<String>,
    },
}

impl Location {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("oci:") {
            if rest.is_empty() {
                bail!("oci: location needs a path");
            }
            return Ok(Location::OciDir(PathBuf::from(rest)));
        }
        if let Some(rest) = s.strip_prefix("docker://") {
            // An @digest ref is the artifact's identity, immutable: split
            // it off first so the tag logic below cannot eat it.
            let (rest, digest) = match rest.rfind('@') {
                Some(at) => (&rest[..at], Some(rest[at + 1..].to_string())),
                None => (rest, None),
            };
            // an explicit scheme wins over the localhost default; it
            // must be stripped before the slash split
            let (scheme, rest) = match rest {
                r if let Some(host) = r.strip_prefix("http://") => (Some("http"), host),
                r if let Some(host) = r.strip_prefix("https://") => (Some("https"), host),
                r => (None, r),
            };
            // Registry = up to the first slash; the tag is a trailing colon
            // in the repository part (a registry port lives before it).
            let (registry, repository) = rest
                .split_once('/')
                .ok_or_else(|| anyhow!("docker:// location needs <registry>/<repo>: {s}"))?;
            let (repository, tag) = match repository.rfind(':') {
                Some(i) => (&repository[..i], Some(repository[i + 1..].to_string())),
                None => (repository, None),
            };
            if registry.is_empty() || repository.is_empty() {
                bail!("malformed docker:// location: {s}");
            }
            return Ok(Location::Registry {
                registry: registry.to_string(),
                repository: repository.to_string(),
                tag,
                digest,
                scheme: scheme.map(str::to_string),
            });
        }
        bail!("unsupported artifact location {s:?} (want oci: or docker://)")
    }

    /// What to hand to `flatpak install` as the image location. The
    /// digest form when we have it: flatpak then pulls the exact bytes
    /// that were verified, not whatever the tag points at now.
    /// (Registry artifacts the client installs go through
    /// [download_layout] instead: flatpak's docker:// path cannot
    /// authenticate, so the client fetches the layout and installs
    /// from `oci:`.)
    pub fn install_arg(&self) -> String {
        match self {
            Location::OciDir(path) => format!("oci:{}", path.display()),
            Location::Registry {
                registry,
                repository,
                tag,
                digest,
                scheme: _,
            } => {
                let repo = if registry.contains('/') {
                    repository.to_string()
                } else {
                    format!("{registry}/{repository}")
                };
                if let Some(digest) = digest {
                    format!("docker://{repo}@{digest}")
                } else {
                    match tag {
                        Some(tag) => format!("docker://{repo}:{tag}"),
                        None => format!("docker://{repo}"),
                    }
                }
            }
        }
    }
}

/// Fetch and verify an artifact. `pinned_digest`, when set, must match the
/// manifest digest actually served.
pub fn verify(location: &Location, pinned_digest: Option<&str>) -> Result<VerifyReport> {
    match location {
        Location::OciDir(dir) => verify_local(dir, pinned_digest),
        Location::Registry {
            registry,
            repository,
            tag,
            digest,
            scheme,
        } => {
            // the location's own digest, when present, is the strongest
            // pin and must agree with the catalog's
            if let (Some(located), Some(pinned)) = (digest.as_deref(), pinned_digest)
                && !located.eq_ignore_ascii_case(pinned)
            {
                bail!("artifact location pins {located}, catalog pins {pinned}");
            }
            verify_registry(
                registry,
                repository,
                scheme.as_deref(),
                digest.as_deref().or(tag.as_deref()),
                pinned_digest,
            )
        }
    }
}

fn verify_local(dir: &Path, pinned: Option<&str>) -> Result<VerifyReport> {
    let index_path = dir.join("index.json");
    let index_bytes = std::fs::read(&index_path)
        .with_context(|| format!("reading {}", index_path.display()))?;
    let index: serde_json::Value = serde_json::from_slice(&index_bytes)
        .with_context(|| format!("parsing {}", index_path.display()))?;
    let manifests = index
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow!("no manifests array in {}", index_path.display()))?;

    let entry = match manifests.len() {
        1 => &manifests[0],
        0 => bail!("empty index in {}", dir.display()),
        _ => bail!(
            "multiple manifests in {}; pass a reference (not supported yet)",
            dir.display()
        ),
    };
    let claimed = entry
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| anyhow!("index entry without digest"))?;
    let bytes = read_local_blob(dir, claimed)?;
    let digest = sha256_digest(&bytes);
    check_pin(pinned, &digest)?;
    let mut report = report_from_manifest(&bytes, &digest)?;
    report.manifest_size = bytes.len() as u64;
    // The config blob carries the flatpak labels; read it locally with the
    // same integrity check the registry path gets from the digest URL.
    if let Some(config_digest) = report.config_digest.clone() {
        let config_bytes = read_local_blob(dir, &config_digest)?;
        apply_config_labels(&config_bytes, &mut report);
    }
    Ok(report)
}

fn verify_registry(
    registry: &str,
    repository: &str,
    scheme: Option<&str>,
    reference: Option<&str>,
    pinned: Option<&str>,
) -> Result<VerifyReport> {
    let base = registry_base(registry, scheme);
    let agent = agent();
    let mut fetch = RegistryFetch::new(&agent, repository);
    let (manifest_bytes, digest) =
        resolve_manifest(&mut fetch, &base, repository, reference, pinned)?;
    let mut report = report_from_manifest(&manifest_bytes, &digest)?;
    report.manifest_size = manifest_bytes.len() as u64;
    // Config and layers come through the same registry.
    finish_registry_report(&mut fetch, &base, repository, &mut report)?;
    Ok(report)
}

/// The manifest bytes a registry reference resolves to. An explicit
/// reference rides as given; otherwise the pin (immutable, so what we
/// verify is exactly what the pin names); otherwise `latest`. A
/// reference that resolves to an index selects this machine's arch
/// entry. The pin check runs against the final manifest's digest.
fn resolve_manifest(
    fetch: &mut RegistryFetch<'_>,
    base: &str,
    repository: &str,
    reference: Option<&str>,
    pinned: Option<&str>,
) -> Result<(Vec<u8>, String)> {
    let reference = reference
        .map(str::to_string)
        .or_else(|| pinned.map(str::to_string))
        .unwrap_or_else(|| "latest".to_string());
    let url = format!("{base}/v2/{repository}/manifests/{reference}");
    let bytes = fetch.get(&url)?;

    // A tag may resolve to an index; select this machine's arch entry.
    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing manifest from {url}"))?;
    if parsed.get("manifests").is_some() {
        let wanted = arch_names();
        let entry = parsed["manifests"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|e| entry_arch(e).is_some_and(|a| wanted.contains(&a)))
            })
            .ok_or_else(|| anyhow!("no manifest for {:?} in index at {url}", wanted))?;
        let entry_digest = entry
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| anyhow!("index entry without digest at {url}"))?;
        let manifest_url = format!("{base}/v2/{repository}/manifests/{entry_digest}");
        let manifest_bytes = fetch.get(&manifest_url)?;
        let digest = sha256_digest(&manifest_bytes);
        check_pin(pinned, &digest)?;
        return Ok((manifest_bytes, digest));
    }

    let digest = sha256_digest(&bytes);
    check_pin(pinned, &digest)?;
    Ok((bytes, digest))
}

/// Download a registry artifact into a local OCI image layout under
/// `dest`: the manifest (checked against `pinned` before anything
/// large moves), then the config and layer blobs, each streamed and
/// hashed against its own digest. Ends with `index.json` and
/// `oci-layout`, the exact shape `flatpak install oci:<dir>` reads.
/// Returns the manifest digest. Registry locations only: an `oci:`
/// location is already local bytes.
pub fn download_layout(location: &Location, pinned: Option<&str>, dest: &Path) -> Result<String> {
    let (registry, repository, scheme, reference) = match location {
        Location::Registry {
            registry,
            repository,
            digest,
            tag,
            scheme,
        } => (registry, repository, scheme, digest.as_deref().or(tag.as_deref())),
        Location::OciDir(_) => {
            bail!("nothing to download: the location is already a local layout")
        }
    };
    let base = registry_base(registry, scheme.as_deref());
    let agent = agent();
    let mut fetch = RegistryFetch::new(&agent, repository);
    let (manifest_bytes, digest) =
        resolve_manifest(&mut fetch, &base, repository, reference, pinned)?;

    let parsed: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parsing manifest {digest}"))?;
    let config_digest = parsed["config"]["digest"]
        .as_str()
        .ok_or_else(|| anyhow!("manifest {digest} carries no config digest"))?;
    let layers: Vec<(String, u64)> = parsed["layers"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|layer| {
            let digest = layer.get("digest")?.as_str()?.to_string();
            let size = layer.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            Some((digest, size))
        })
        .collect();

    // The layout is content-addressed by manifest digest: a completed
    // twin means a previous run got it all, and rename-in makes the
    // last step atomic.
    let staging = dest.with_extension("part");
    let _ = std::fs::remove_dir_all(&staging);
    let blobs = staging.join("blobs/sha256");
    std::fs::create_dir_all(&blobs).with_context(|| format!("creating {}", staging.display()))?;
    let result = (|| -> Result<()> {
        std::fs::write(blobs.join(digest.trim_start_matches("sha256:")), &manifest_bytes)?;
        fetch_blob_to(&mut fetch, &base, repository, config_digest, &blobs)?;
        for (layer_digest, _size) in &layers {
            fetch_blob_to(&mut fetch, &base, repository, layer_digest, &blobs)?;
        }
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": parsed["mediaType"].as_str().unwrap_or(MEDIA_MANIFEST_OCI),
                "digest": digest,
                "size": manifest_bytes.len(),
            }],
        });
        std::fs::write(staging.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
        std::fs::write(
            staging.join("oci-layout"),
            "{\"imageLayoutVersion\":\"1.0.0\"}\n",
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&staging, dest)
                .with_context(|| format!("moving {} into place", staging.display()))?;
            Ok(digest)
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(err)
        }
    }
}

/// Fetch one blob (config or layer) by digest into a layout's blob
/// directory, hashing the stream on the way: a byte that does not
/// match the digest it was named by never lands.
fn fetch_blob_to(
    fetch: &mut RegistryFetch<'_>,
    base: &str,
    repository: &str,
    digest: &str,
    blobs: &Path,
) -> Result<()> {
    let url = format!("{base}/v2/{repository}/blobs/{digest}");
    let response = fetch.get_response(&url)?;
    let mut reader = response.into_reader();
    let final_path = blobs.join(digest.trim_start_matches("sha256:"));
    let tmp = final_path.with_extension("part");
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut reader, &mut buffer)
            .with_context(|| format!("reading body of {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    let hashed: String = format!(
        "sha256:{}",
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    if hashed != digest {
        let _ = std::fs::remove_file(&tmp);
        bail!("{url} served {hashed}, named {digest}");
    }
    std::fs::rename(&tmp, &final_path)
        .with_context(|| format!("moving {} into place", tmp.display()))?;
    Ok(())
}

/// The OCI manifest media type, the default `index.json` entry type
/// (ghcr serves these; a docker-typed manifest keeps its own string).
const MEDIA_MANIFEST_OCI: &str = "application/vnd.oci.image.manifest.v1+json";

fn finish_registry_report(
    fetch: &mut RegistryFetch<'_>,
    base: &str,
    repository: &str,
    report: &mut VerifyReport,
) -> Result<()> {
    // Layers are only reachable as blobs; we do not download them here
    // (flatpak does at install), but the config blob carries the labels.
    if let Some(config_digest) = report.config_digest.clone() {
        let url = format!("{base}/v2/{repository}/blobs/{config_digest}");
        let config_bytes = fetch.get(&url)?;
        apply_config_labels(&config_bytes, report);
    }
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

/// Local registries speak plain http; everything else is https.
fn registry_base(registry: &str, scheme: Option<&str>) -> String {
    let scheme = scheme.map(str::to_string).unwrap_or_else(|| {
        let local = registry.starts_with("localhost:")
            || registry.starts_with("127.0.0.1:")
            || registry.starts_with("[::1]:");
        if local { "http" } else { "https" }.to_string()
    });
    format!("{scheme}://{registry}")
}

/// A registry fetcher that speaks the standard token challenge. A 401
/// whose `WWW-Authenticate` advertises `Bearer realm="..."` is an
/// invitation to fetch a (possibly anonymous) token and retry; ghcr,
/// Docker Hub, and quay all run this flow even for public repos. The
/// token is fetched lazily once and reused across the manifest, index
/// entry, and config fetches of one verify.
struct RegistryFetch<'a> {
    agent: &'a ureq::Agent,
    repository: &'a str,
    token: Option<String>,
}

impl<'a> RegistryFetch<'a> {
    fn new(agent: &'a ureq::Agent, repository: &'a str) -> Self {
        RegistryFetch {
            agent,
            repository,
            token: None,
        }
    }

    fn get(&mut self, url: &str) -> Result<Vec<u8>> {
        let response = self.get_response(url)?;
        read_body(response.into_reader(), url)
    }

    /// The raw response, through the challenge round: callers that
    /// stream (artifact blobs) read the body themselves.
    fn get_response(&mut self, url: &str) -> Result<ureq::Response> {
        match self.send(url, self.token.as_deref()) {
            // not a status error at all: connection, timeout, ...
            Err(ureq::Error::Transport(t)) => Err(anyhow!("GET {url}: {t}")),
            Err(ureq::Error::Status(401, response)) => {
                // one challenge round: fetch a token from the challenge
                // and retry once, so a real denial costs one round trip
                if self.token.is_none()
                    && let Some(challenge) = response.header("www-authenticate")
                {
                    let client = format!("orchard/{}", env!("CARGO_PKG_VERSION"));
                    self.token = Some(token_from_challenge(
                        challenge,
                        self.repository,
                        &client,
                    )?);
                    return self
                        .send(url, self.token.as_deref())
                        .map_err(|err| anyhow!("GET {url} after token: {err}"));
                }
                Err(anyhow!("GET {url}: status code 401"))
            }
            Err(ureq::Error::Status(code, _)) => Err(anyhow!("GET {url}: status code {code}")),
            Ok(response) => Ok(response),
        }
    }

    fn send(
        &self,
        url: &str,
        token: Option<&str>,
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut request = self.agent.get(url).set("Accept", MANIFEST_ACCEPT);
        if let Some(token) = token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        request.call()
    }
}

fn read_body(reader: impl std::io::Read, url: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut { reader }, &mut bytes)
        .with_context(|| format!("reading body of {url}"))?;
    Ok(bytes)
}

/// Turn a `WWW-Authenticate: Bearer realm="…",service="…",scope="…"`
/// challenge into a token. The scope pins the pull to this one
/// repository; the challenge's own scope is fine when present, since
/// it names the same repo. Anonymous (no credentials) is the norm for
/// public repos: the endpoint hands out an unprivileged token. The
/// token endpoint takes query params and answers JSON with `token` or
/// `access_token`; values are percent-encoded so hosts and repo names
/// with unusual characters survive.
fn token_from_challenge(challenge: &str, repository: &str, client_id: &str) -> Result<String> {
    let (realm, token_url) = token_endpoint(challenge, repository, client_id)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let response = agent
        .get(&token_url)
        .set("Accept", "application/json")
        .call()
        .with_context(|| format!("GET token endpoint {realm}"))?;
    let record: serde_json::Value = serde_json::from_reader(response.into_reader())
        .with_context(|| format!("parsing token response from {realm}"))?;
    let token = record
        .get("token")
        .or_else(|| record.get("access_token"))
        .and_then(|token| token.as_str())
        .ok_or_else(|| anyhow!("token response from {realm} carries no token"))?;
    if token.is_empty() {
        bail!("token response from {realm} carries an empty token");
    }
    Ok(token.to_string())
}

/// The pure half of [`token_from_challenge`]: the realm and the fully
/// formed token URL for a challenge.
fn token_endpoint(challenge: &str, repository: &str, client_id: &str) -> Result<(String, String)> {
    let mut realm = None;
    let mut service = None;
    let mut scope = Some(format!("repository:{repository}:pull"));
    // key="value" pairs; the scheme word and the first pair share the
    // first comma segment ("Bearer realm=\"...\""), so drop the scheme
    // word rather than the segment
    for (index, part) in challenge.split(',').enumerate() {
        let part = if index == 0 {
            match part.trim().split_once(char::is_whitespace) {
                Some((_scheme, rest)) => rest,
                None => continue,
            }
        } else {
            part.trim()
        };
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("malformed WWW-Authenticate entry {part:?}"))?;
        let value = value.trim_matches('"');
        match key.trim() {
            "realm" => realm = Some(value.to_string()),
            "service" => service = Some(value.to_string()),
            "scope" => scope = Some(value.to_string()),
            _ => {}
        }
    }
    let realm = realm.ok_or_else(|| anyhow!("WWW-Authenticate without realm: {challenge:?}"))?;
    let query: &[(&str, &str)] = &[
        ("service", service.as_deref().unwrap_or("")),
        ("scope", scope.as_deref().unwrap_or("")),
        ("client_id", client_id),
    ];
    let encoded: Vec<String> = query
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                form_encode_component(key),
                form_encode_component(value)
            )
        })
        .collect();
    let token_url = format!("{}?{}", realm, encoded.join("&"));
    Ok((realm, token_url))
}

/// One `application/x-www-form-urlencoded` component: everything
/// outside RFC 3986's unreserved set gets %XX.
fn form_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Architecture aliases flatpak and OCI use for the same machine.
fn arch_names() -> Vec<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => vec!["amd64", "x86_64"],
        "aarch64" => vec!["arm64", "aarch64"],
        other => vec![other],
    }
}

fn entry_arch(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("platform")
        .and_then(|p| p.get("architecture"))
        .and_then(|a| a.as_str())
}

/// Build the report from manifest bytes: config digest, layer sizes.
fn report_from_manifest(manifest_bytes: &[u8], digest: &str) -> Result<VerifyReport> {
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes)
        .with_context(|| format!("parsing manifest {digest}"))?;
    let config_digest = manifest
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
        .map(str::to_string);
    let download_size = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .map(|layers| {
            layers
                .iter()
                .filter_map(|l| l.get("size").and_then(|s| s.as_u64()))
                .sum()
        })
        .unwrap_or(0);
    Ok(VerifyReport {
        digest: digest.to_string(),
        manifest_size: 0,
        flatpak_ref: None,
        metadata: None,
        installed_size: None,
        download_size,
        config_digest,
    })
}

/// Fill label-derived fields from the image config blob.
fn apply_config_labels(config_bytes: &[u8], report: &mut VerifyReport) {
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(config_bytes) else {
        return;
    };
    // Docker-style capital Labels; accept lowercase too.
    let labels = config
        .get("config")
        .and_then(|c| c.get("Labels").or_else(|| c.get("labels")))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let get = |key: &str| -> Option<String> {
        labels
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    report.flatpak_ref = get("org.flatpak.ref");
    report.metadata = get("org.flatpak.metadata");
    report.installed_size = get("org.flatpak.installed-size")
        .and_then(|s| s.parse().ok());
}

/// Read a blob from an OCI layout, verifying its content matches the digest
/// in its path name.
fn read_local_blob(dir: &Path, digest: &str) -> Result<Vec<u8>> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("only sha256 digests are supported: {digest}"))?;
    let path = dir.join("blobs/sha256").join(hex);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let actual = sha256_digest(&bytes);
    if actual != digest {
        bail!("blob integrity failure: {} hashes to {actual}", path.display());
    }
    Ok(bytes)
}

fn check_pin(pinned: Option<&str>, actual: &str) -> Result<()> {
    match pinned {
        Some(pinned) if pinned != actual => Err(anyhow!(
            "digest mismatch: catalog pins {pinned}, registry served {actual}"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_PIN: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// Unique per test: tests run in parallel and must not share a layout.
    fn unique_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "orchard-oci-test-{}-{name}",
            std::process::id()
        ))
    }

    /// Build a minimal but valid OCI layout in a temp dir: one config, one
    /// gzip layer, one manifest, index.json.
    fn make_layout(name: &str) -> (PathBuf, String) {
        let dir = unique_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();

        let config = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Labels": {
                    "org.flatpak.ref": "app/com.example.Hello/x86_64/stable",
                    "org.flatpak.metadata": "[Application]\nname=com.example.Hello\n"
                }
            },
            "rootfs": {"type": "layers", "diff_ids": ["sha256:aa"]}
        });
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_digest = sha256_digest(&config_bytes);

        let layer_bytes = b"layer-bytes".to_vec();
        let layer_digest = sha256_digest(&layer_bytes);

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_bytes.len()
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = sha256_digest(&manifest_bytes);

        for (digest, bytes) in [
            (&config_digest, &config_bytes),
            (&layer_digest, &layer_bytes),
            (&manifest_digest, &manifest_bytes),
        ] {
            let hex = digest.strip_prefix("sha256:").unwrap();
            std::fs::write(blobs.join(hex), bytes).unwrap();
        }

        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_bytes.len()
            }]
        });
        std::fs::write(dir.join("index.json"), serde_json::to_vec(&index).unwrap())
            .unwrap();
        std::fs::write(
            dir.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        (dir, manifest_digest)
    }

    /// Live probe of the token-challenge flow against ghcr: anonymous
    /// pull of the v0.1.0 x86_64 artifact, verified against its signed
    /// pin, then downloaded into a layout and verified again as local
    /// bytes (the client's real install path: flatpak's docker://
    /// fetch cannot authenticate, so it installs from this layout).
    /// Ignored by default (network, and the digest is release
    /// specific); run with `--ignored` when touching registry code.
    #[test]
    #[ignore]
    fn verifies_ghcr_through_the_token_challenge() {
        let location =
            Location::parse("docker://ghcr.io/letdown2491/orchard").unwrap();
        let pin = "sha256:151fce148d463871c07a28342dc9064c6e52ce0cc5a20ecca99311811888753f";
        let report = verify(&location, Some(pin)).unwrap();
        assert_eq!(
            report.flatpak_ref.as_deref(),
            Some("app/rocks.orchard.Orchard/x86_64/stable")
        );

        // the download path: a fresh layout, verified as local bytes
        let dest = unique_dir("ghcr-download");
        let digest = download_layout(&location, Some(pin), &dest).unwrap();
        assert_eq!(digest, pin);
        assert!(dest.join("index.json").is_file(), "index.json written");
        assert!(dest.join("oci-layout").is_file(), "oci-layout written");
        let report = verify(&Location::OciDir(dest.clone()), Some(pin)).unwrap();
        assert_eq!(
            report.flatpak_ref.as_deref(),
            Some("app/rocks.orchard.Orchard/x86_64/stable")
        );
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn refuses_to_download_a_local_layout() {
        let (dir, _digest) = make_layout("no-download");
        let dest = unique_dir("no-download-dest");
        let err = download_layout(&Location::OciDir(dir.clone()), None, &dest)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("already a local layout"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verifies_local_layout_and_reads_labels() {
        let (dir, digest) = make_layout("verify");
        let report = verify(&Location::OciDir(dir.clone()), Some(&digest)).unwrap();
        assert_eq!(report.digest, digest);
        assert_eq!(
            report.flatpak_ref.as_deref(),
            Some("app/com.example.Hello/x86_64/stable")
        );
        assert!(report.metadata.as_deref().unwrap_or("").starts_with("[Application]"));
        assert_eq!(report.download_size, 11);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_pin_mismatch() {
        let (dir, _digest) = make_layout("pin");
        let err = verify(&Location::OciDir(dir.clone()), Some("sha256:bad"))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("digest mismatch"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_tampered_blob() {
        let (dir, digest) = make_layout("tamper");
        // Flip a byte in the config blob.
        let hex = {
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    dir.join("blobs/sha256")
                        .join(digest.strip_prefix("sha256:").unwrap()),
                )
                .unwrap(),
            )
            .unwrap();
            manifest["config"]["digest"]
                .as_str()
                .unwrap()
                .strip_prefix("sha256:")
                .unwrap()
                .to_string()
        };
        let path = dir.join("blobs/sha256").join(&hex);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert!(verify(&Location::OciDir(dir.clone()), None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_ghcr_style_challenge() {
        let challenge = "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:letdown2491/orchard:pull\"";
        let (realm, url) =
            token_endpoint(challenge, "letdown2491/orchard", "orchard/test").unwrap();
        assert_eq!(realm, "https://ghcr.io/token");
        // the challenge's own scope wins; client id rides along
        assert!(url.contains("service=ghcr.io"), "{url}");
        assert!(
            url.contains("scope=repository%3Aletdown2491%2Forchard%3Apull"),
            "{url}"
        );
        assert!(url.contains("client_id=orchard%2Ftest"), "{url}");

        // no scope in the challenge: the repository-derived fallback
        let (realm, url) = token_endpoint(
            "Bearer realm=\"https://auth.docker.io/token\",service=\"registry.docker.io\"",
            "owner/repo",
            "orchard/test",
        )
        .unwrap();
        assert_eq!(realm, "https://auth.docker.io/token");
        assert!(url.contains("scope=repository%3Aowner%2Frepo%3Apull"), "{url}");

        // a challenge without a realm is a hard error
        assert!(token_endpoint("Bearer service=\"x\"", "owner/repo", "c").is_err());
    }

    #[test]
    fn parses_locations() {
        let loc = Location::parse("docker://ghcr.io/owner/repo:1.2").unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "ghcr.io".into(),
                repository: "owner/repo".into(),
                tag: Some("1.2".into()),
                digest: None,
                scheme: None
            }
        );
        assert_eq!(loc.install_arg(), "docker://ghcr.io/owner/repo:1.2");

        // Host with port keeps the port in the registry part.
        let loc = Location::parse("docker://localhost:5000/hello:stable").unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "localhost:5000".into(),
                repository: "hello".into(),
                tag: Some("stable".into()),
                digest: None,
                scheme: None
            }
        );

        // an explicit scheme overrides the localhost default
        let loc = Location::parse("docker://https://localhost:5000/hello").unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "localhost:5000".into(),
                repository: "hello".into(),
                tag: None,
                digest: None,
                scheme: Some("https".into())
            }
        );
        assert_eq!(loc.install_arg(), "docker://localhost:5000/hello");

        // digest refs split off before the tag logic sees them
        let digest_ref = format!(
            "docker://localhost:5000/hello@{}",
            DIGEST_PIN
        );
        let loc = Location::parse(&digest_ref).unwrap();
        assert_eq!(
            loc,
            Location::Registry {
                registry: "localhost:5000".into(),
                repository: "hello".into(),
                tag: None,
                digest: Some(DIGEST_PIN.into()),
                scheme: None
            }
        );
        assert_eq!(loc.install_arg(), digest_ref);

        let loc = Location::parse("oci:/some/dir").unwrap();
        assert_eq!(loc.install_arg(), "oci:/some/dir");

        assert!(Location::parse("https://nope").is_err());
    }

    #[test]
    fn local_registries_use_http() {
        assert_eq!(
            registry_base("localhost:5000", None),
            "http://localhost:5000"
        );
        assert_eq!(
            registry_base("ghcr.io", None),
            "https://ghcr.io"
        );
        assert_eq!(
            registry_base("localhost:5000", Some("https")),
            "https://localhost:5000"
        );
    }
}
