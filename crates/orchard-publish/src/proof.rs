//! Repository proof: write the publisher's hex key to `orchard.txt`
//! at the repository's own forge, so the listing's `repository` tag
//! stops being a free claim. The proof URL is derived from the tag
//! itself, so claiming a repository means hosting the file at the
//! forge's URL: an impersonator must control the real repo.
//!
//! GitHub today, via the contents API: create, or update when the
//! file exists (the sha of the current file is required). `HEAD` on
//! the raw host resolves the default branch at read time, so no
//! branch bookkeeping is needed here.

use anyhow::{anyhow, bail, Context as _, Result};

/// The `owner/repo` a manifest's repository tag names, when it is a
/// github repo: the proof target. Other forges have no proof path
/// yet, and pretending would be a guess.
pub fn github_repo(repository: &str) -> Option<String> {
    let rest = repository
        .strip_prefix("https://")
        .or_else(|| repository.strip_prefix("http://"))?
        .trim_start_matches("www.");
    let rest = rest.strip_prefix("github.com/")?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The proof file: a human header plus the keys, one per line. The
/// npub form is written (what the UI shows, and bech32 carries a
/// checksum); the client's parser accepts hex and npub alike.
pub fn proof_content(pubkey_npub: &str) -> String {
    format!(
        "# Orchard repository proof: this file is read by Orchard clients to\n\
         # verify that the publisher key of a listing naming this repository\n\
         # controls this repo. One key per line (npub or hex).\n\
         {pubkey_npub}\n"
    )
}

/// Write the proof file to `owner/repo` with a token that has
/// `contents:write`. Existing-file updates need the current sha.
pub fn write_proof(repo: &str, pubkey_npub: &str, token: &str) -> Result<()> {
    let api = format!("https://api.github.com/repos/{repo}/contents/orchard.txt");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();

    let sha: Option<String> = agent
        .get(&api)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()
        .and_then(|response| response.into_json::<serde_json::Value>().ok())
        .and_then(|record| {
            record
                .get("sha")
                .and_then(|sha| sha.as_str().map(str::to_string))
        });

    let mut body = serde_json::json!({
        "message": "orchard: refresh the publisher proof file",
        "content": base64(proof_content(pubkey_npub).as_bytes()),
    });
    if let Some(sha) = sha {
        body["sha"] = serde_json::json!(sha);
    }

    let response = agent
        .put(&api)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .send_json(body)
        .map_err(|err| anyhow!("writing the proof file: {err}"))
        .with_context(|| format!("into {repo}"))?;
    // 200 is an update of the existing file, 201 Created its first
    // write (the matrix leg that runs first creates it): both are
    // success. Anything else is a denial.
    if !matches!(response.status(), 200 | 201) {
        bail!("the forge answered {} writing the proof file", response.status());
    }
    Ok(())
}

/// Standard base64 with padding; the contents API wants it for the
/// file body.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let word = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(word >> 18) as usize & 63] as char);
        out.push(ALPHABET[(word >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(word >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[word as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn proof_content_carries_the_key_on_its_own_line() {
        // the npub the CLI writes: the client's parser takes it
        // structurally and the client converts through the codec
        let npub = "npub1wqmqjss4wszcywfjhmgxvjq4jsm3cjsm5u9qk7zqzeuflqhv4cvsdpzmx2";
        let content = proof_content(npub);
        let key_line = content.lines().last().expect("key line");
        assert_eq!(key_line, npub);
        let parsed = orchard_core::proof::parse_proof_keys(&content);
        assert_eq!(
            parsed,
            Some(vec![orchard_core::proof::ProofKey::Npub(npub.to_string())])
        );
    }
}
