//! PKCE (RFC 7636) code verifier + S256 challenge generation.
//!
//! Port of the `generatePKCE` / `base64UrlEncode` helpers in opencode
//! `packages/opencode/src/plugin/openai/codex.ts`. The verifier is a
//! base64url (no padding) string in the RFC-mandated 43..=128 length range;
//! the challenge is `base64url(sha256(verifier))`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A PKCE verifier/challenge pair. Port of the `PkceCodes` interface in
/// `codex.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// The high-entropy code verifier (base64url, 43..=128 chars).
    pub verifier: String,
    /// The S256 code challenge = `base64url(sha256(verifier))`.
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh verifier/challenge pair.
    ///
    /// 64 random bytes are base64url-encoded to an 86-char verifier (inside
    /// the RFC 7636 43..=128 window), then the S256 challenge is derived.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 64];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// Compute the S256 challenge for a given verifier:
/// `base64url_no_pad(sha256(verifier))`.
///
/// Deterministic — the same verifier always yields the same challenge. Port of
/// the `challenge` computation in `codex.ts` `generatePKCE`.
#[must_use]
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}
