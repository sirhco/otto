//! PKCE: verifier length/charset; challenge == base64url(sha256(verifier));
//! deterministic given a fixed verifier.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use otto_auth::{Pkce, challenge_for};
use sha2::{Digest, Sha256};

fn is_base64url(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[test]
fn verifier_length_and_charset() {
    for _ in 0..50 {
        let pkce = Pkce::generate();
        let len = pkce.verifier.len();
        assert!(
            (43..=128).contains(&len),
            "verifier length {len} out of RFC 7636 range"
        );
        assert!(is_base64url(&pkce.verifier), "verifier not base64url");
        assert!(is_base64url(&pkce.challenge), "challenge not base64url");
    }
}

#[test]
fn challenge_is_s256_of_verifier() {
    let pkce = Pkce::generate();
    let digest = Sha256::digest(pkce.verifier.as_bytes());
    let expected = URL_SAFE_NO_PAD.encode(digest);
    assert_eq!(pkce.challenge, expected);
}

#[test]
fn challenge_for_is_deterministic() {
    let verifier = "fixed-verifier-value-fixed-verifier-value-01";
    let a = challenge_for(verifier);
    let b = challenge_for(verifier);
    assert_eq!(a, b);

    // Known-answer: base64url(sha256("...")).
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(a, expected);
}
