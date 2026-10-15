//! Registry push, Docker v2 API: blob upload (single PUT, monolithic),
//! then the manifest, then a read-back check. Plain http for localhost
//! registries, https otherwise, matching the client's verify path.
//! Bearer-token challenge/response for the public registries (ghcr.io):
//! on a 401 with a Bearer challenge, fetch a token and ride it on every
//! request. Credentials come from ORCHARD_REGISTRY_USER and
//! ORCHARD_REGISTRY_PASSWORD when set; without them the token request
//! is anonymous.

use std::path::Path;

use anyhow::{bail, Context as _, Result};

use orchard_core::oci::sha256_digest;

const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Push every blob the layout's index references, then the manifest
/// itself. Returns the digest the registry will serve, which must equal
/// the locally computed one or something is lying.
///
/// `<registry>/<repo>` with an optional scheme on the registry part
/// (`https://localhost:5000/hello`): otherwise localhost registries use
/// http and everything else https.
pub fn push_layout(registry_repo: &str, tag: &str, layout: &Path) -> Result<String> {
    let mut scheme = None;
    let mut rest = registry_repo;
    for prefix in ["http://", "https://"] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            scheme = Some(prefix.trim_end_matches("://"));
            rest = stripped;
        }
    }
    let (registry, repository) = rest
        .split_once('/')
        .context("registry must be <registry>/<repo>, e.g. ghcr.io/owner/app")?;
    let base = registry_base(registry, scheme);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build();
    let token = bearer_token(&agent, &base, repository)?;

    // which blobs the index references: config, layers, and the manifest
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(layout.join("index.json"))?)
        .context("reading layout index.json")?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .context("layout index without manifest digest")?
        .to_string();
    let manifest_bytes = read_blob(layout, &manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let mut referenced = Vec::new();
    if let Some(config) = manifest["config"]["digest"].as_str() {
        referenced.push(config.to_string());
    }
    if let Some(layers) = manifest["layers"].as_array() {
        for layer in layers {
            if let Some(digest) = layer["digest"].as_str() {
                referenced.push(digest.to_string());
            }
        }
    }

    for digest in &referenced {
        if blob_present(&agent, &base, repository, digest, token.as_deref())? {
            println!("  {registry}: blob {} already present", &digest[..19.min(digest.len())]);
            continue;
        }
        upload_blob(&agent, &base, repository, &read_blob(layout, digest)?, token.as_deref())?;
        println!("  {registry}: pushed blob {}", &digest[..19.min(digest.len())]);
    }

    // 201 with an empty body on success; ureq already turned 4xx/5xx
    // into errors
    agent
        .put(&format!("{base}/v2/{repository}/manifests/{tag}"))
        .set("Content-Type", MEDIA_MANIFEST)
        .set_bearer(token.as_deref())
        .send_bytes(&manifest_bytes)
        .map_err(|err| anyhow::anyhow!("pushing manifest: {err}"))?;
    println!("  {registry}: pushed manifest {tag}");

    // read back by digest: the pin must be what the registry serves
    let served = agent
        .get(&format!("{base}/v2/{repository}/manifests/{manifest_digest}"))
        .set("Accept", MEDIA_MANIFEST)
        .set_bearer(token.as_deref())
        .call()
        .context("reading back the pushed manifest")?;
    let mut served_bytes = Vec::new();
    std::io::Read::read_to_end(&mut served.into_reader(), &mut served_bytes)?;
    let served_digest = sha256_digest(&served_bytes);
    if served_digest != manifest_digest {
        bail!(
            "registry serves {served_digest} where we pushed {manifest_digest}: refusing to sign"
        );
    }
    Ok(manifest_digest)
}

fn read_blob(layout: &Path, digest: &str) -> Result<Vec<u8>> {
    let path = layout
        .join("blobs/sha256")
        .join(digest.trim_start_matches("sha256:"));
    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

fn blob_present(
    agent: &ureq::Agent,
    base: &str,
    repository: &str,
    digest: &str,
    token: Option<&str>,
) -> Result<bool> {
    match agent
        .head(&format!("{base}/v2/{repository}/blobs/{digest}"))
        .set_bearer(token)
        .call()
    {
        Ok(_) => Ok(true),
        Err(ureq::Error::Status(404, _)) => Ok(false),
        Err(err) => Err(err).context("checking blob"),
    }
}

fn upload_blob(
    agent: &ureq::Agent,
    base: &str,
    repository: &str,
    blob: &[u8],
    token: Option<&str>,
) -> Result<()> {
    let digest = sha256_digest(blob);
    let upload = agent
        .post(&format!("{base}/v2/{repository}/blobs/uploads/"))
        .set_bearer(token)
        .call()
        .context("starting blob upload")?;
    let location = upload
        .header("Location")
        .context("upload response without Location")?
        .to_string();
    // the registry may hand back a relative URL
    let url = if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        location
    };
    let separator = if url.contains('?') { '&' } else { '?' };
    agent
        .put(&format!("{url}{separator}digest={digest}"))
        .set_bearer(token)
        .send_bytes(blob)
        .map_err(|err| anyhow::anyhow!("uploading blob {digest}: {err}"))?;
    Ok(())
}

fn registry_base(registry: &str, scheme: Option<&str>) -> String {
    let scheme = scheme.map(str::to_string).unwrap_or_else(|| {
        let local = registry.starts_with("localhost:")
            || registry.starts_with("127.0.0.1:")
            || registry.starts_with("[::1]:");
        if local { "http" } else { "https" }.to_string()
    });
    format!("{scheme}://{registry}")
}

/// The token to ride on every registry request, when the registry
/// demands one. Probing `/v2/` is the Docker v2 handshake: 200 means
/// no auth (the local demo registry), a 401 with a Bearer challenge
/// means fetch a token from the named realm, anything else is an
/// error worth surfacing.
fn bearer_token(agent: &ureq::Agent, base: &str, repository: &str) -> Result<Option<String>> {
    match agent.get(&format!("{base}/v2/")).call() {
        Ok(_) => Ok(None),
        Err(ureq::Error::Status(401, response)) => {
            let header = response
                .header("WWW-Authenticate")
                .context("401 without WWW-Authenticate")?
                .to_string();
            let challenge = parse_bearer_challenge(&header)
                .with_context(|| format!("unreadable challenge: {header}"))?;
            let mut url = format!(
                "{}?service={}&scope=repository:{repository}:pull,push",
                challenge.realm, challenge.service,
            );
            if let Some(account) = std::env::var("ORCHARD_REGISTRY_USER").ok().filter(|value| !value.is_empty()) {
                url.push_str(&format!("&account={account}"));
            }
            let mut request = agent.get(&url);
            if let (Some(user), Some(pass)) = (
                std::env::var("ORCHARD_REGISTRY_USER").ok().filter(|value| !value.is_empty()),
                std::env::var("ORCHARD_REGISTRY_PASSWORD").ok().filter(|value| !value.is_empty()),
            ) {
                request = request.set("Authorization", &basic_auth(&user, &pass));
            }
            let response = request
                .call()
                .with_context(|| format!("fetching a registry token from {}", challenge.realm))?;
            let body: serde_json::Value = response.into_json().context("token response")?;
            let token = body["token"]
                .as_str()
                .or_else(|| body["access_token"].as_str())
                .with_context(|| {
                    format!(
                        "no token in the registry's response: {}",
                        serde_json::to_string(&body).unwrap_or_default()
                    )
                })?;
            Ok(Some(token.to_string()))
        }
        Err(err) => Err(err).context("probing the registry"),
    }
}

/// `Bearer realm="…",service="…",scope="…"`: comma-separated key="value"
/// pairs, values possibly quoted.
fn parse_bearer_challenge(header: &str) -> Option<Challenge> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut service = String::new();
    for pair in rest.split(',') {
        let pair = pair.trim();
        let (key, value) = pair.split_once('=')?;
        let value = value.trim_matches('"');
        match key {
            "realm" => realm = Some(value.to_string()),
            "service" => service = value.to_string(),
            _ => {}
        }
    }
    Some(Challenge {
        realm: realm?,
        service,
    })
}

struct Challenge {
    realm: String,
    service: String,
}

/// `Authorization: Basic …` for the token endpoint. Standard base64 of
/// `user:pass`, hand-rolled to keep the dependency list at zero.
fn basic_auth(user: &str, pass: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{pass}");
    let mut out = String::from("Basic ");
    for chunk in input.as_bytes().chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let window = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        out.push(ALPHABET[(window >> 18) as usize & 63] as char);
        out.push(ALPHABET[(window >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(window >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[window as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Attach the Authorization header when a token exists.
trait SetBearer {
    fn set_bearer(self, token: Option<&str>) -> Self;
}

impl SetBearer for ureq::Request {
    fn set_bearer(self, token: Option<&str>) -> Self {
        match token {
            Some(token) => self.set("Authorization", &format!("Bearer {token}")),
            None => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_wins_over_localhost_default() {
        assert_eq!(registry_base("localhost:5000", None), "http://localhost:5000");
        assert_eq!(registry_base("ghcr.io", None), "https://ghcr.io");
        assert_eq!(
            registry_base("localhost:5000", Some("https")),
            "https://localhost:5000"
        );
    }

    #[test]
    fn challenge_parses_realm_and_service() {
        // ghcr.io's exact shape
        let challenge = parse_bearer_challenge(
            "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\"",
        )
        .expect("parses");
        assert_eq!(challenge.realm, "https://ghcr.io/token");
        assert_eq!(challenge.service, "ghcr.io");
        // scope may or may not ride along; we always request our own
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
        assert!(parse_bearer_challenge("Bearer").is_none());
    }

    #[test]
    fn basic_auth_encodes_user_colon_pass() {
        // the classic RFC 4648 test vectors through this path
        assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
        // padding when the input length hits 4k+1 and 4k+2 byte counts
        assert_eq!(basic_auth("a", "b"), "Basic YTpi");
        assert_eq!(basic_auth("a", ""), "Basic YTo=");
        // ghcr with a GITHUB_TOKEN: the actor name and a long token
        let header = basic_auth("Letdown2491", "ghs_0123456789abcdef");
        assert!(header.starts_with("Basic "));
        let decoded = header
            .strip_prefix("Basic ")
            .and_then(base64_decode)
            .expect("decodes");
        assert_eq!(decoded, b"Letdown2491:ghs_0123456789abcdef");
    }

    /// Test helper: minimal base64 decoder for the encoder's output.
    fn base64_decode(encoded: &str) -> Option<Vec<u8>> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let bytes: Vec<u8> = encoded
            .bytes()
            .filter(|b| *b != b'=')
            .map(|b| {
                ALPHABET
                    .iter()
                    .position(|a| *a == b)
                    .map(|i| i as u8)
                    .unwrap_or(64)
            })
            .collect();
        if bytes.iter().any(|b| *b > 63) {
            return None;
        }
        for chunk in bytes.chunks(4) {
            let window = (u32::from(chunk[0]) << 18)
                | (u32::from(chunk[1]) << 12)
                | (u32::from(*chunk.get(2).unwrap_or(&0)) << 6)
                | u32::from(*chunk.get(3).unwrap_or(&0));
            out.push((window >> 16) as u8);
            if chunk.len() > 2 {
                out.push((window >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(window as u8);
            }
        }
        Some(out)
    }
}

/// The NIP-82 asset URL identifies the immutable manifest bytes in the registry.
pub fn manifest_url(registry_repo: &str, digest: &str) -> Result<String> {
    orchard_core::oci_asset::hash_hex(digest)?;
    let (scheme, rest) = if let Some(rest) = registry_repo.strip_prefix("https://") {
        (Some("https"), rest)
    } else if let Some(rest) = registry_repo.strip_prefix("http://") {
        (Some("http"), rest)
    } else { (None, registry_repo) };
    let (registry, repository) = rest.split_once('/').context("registry needs host/repository")?;
    Ok(format!("{}/v2/{repository}/manifests/{digest}", registry_base(registry, scheme)))
}

/// Config and layer endpoints share the registry repository and blob digest.
pub fn blob_url(registry_repo: &str, digest: &str) -> Result<String> {
    let url = manifest_url(registry_repo, digest)?;
    let (base, hash) = url.rsplit_once("/manifests/").context("missing manifest endpoint")?;
    Ok(format!("{base}/blobs/{hash}"))
}

/// Convert an immutable registry manifest URL into the location v0.2.0 reads.
/// Direct blob hosts cannot provide this registry compatibility path.
pub fn legacy_location(url: &str, hash: &str) -> Result<String> {
    let (scheme, rest) = url.split_once("://").context("manifest URL needs a scheme")?;
    if !matches!(scheme, "http" | "https") { bail!("manifest URL must use HTTP or HTTPS"); }
    let (host, path) = rest.split_once("/v2/").context("--legacy-oci requires registry manifest URLs")?;
    let (repository, digest) = path.rsplit_once("/manifests/").context("--legacy-oci requires registry manifest URLs")?;
    if host.is_empty() || repository.is_empty() || orchard_core::oci_asset::hash_hex(digest)? != hash {
        bail!("legacy registry location differs from manifest asset");
    }
    Ok(format!("docker://{scheme}://{host}/{repository}@sha256:{hash}"))
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    #[test]
    fn legacy_location_requires_a_matching_registry_manifest() {
        let hash = "a".repeat(64);
        assert_eq!(legacy_location(&format!("https://ghcr.io/v2/owner/app/manifests/sha256:{hash}"), &hash).unwrap(),
            format!("docker://https://ghcr.io/owner/app@sha256:{hash}"));
        assert!(legacy_location(&format!("https://blossom.example/{hash}"), &hash).is_err());
        assert!(legacy_location(&format!("https://ghcr.io/v2/owner/app/manifests/sha256:{hash}"), &"b".repeat(64)).is_err());
    }
}
