//! AWS Signature Version 4 request signer.
//!
//! Hand-rolled implementation of the [AWS SigV4 signing process], used to
//! authenticate requests to Amazon Bedrock. There is no opencode source to
//! port here — opencode delegates SigV4 signing to a third-party library —
//! so this is a direct implementation of the published AWS algorithm:
//! canonical request → string-to-sign → HMAC signing-key chain →
//! `Authorization` header.
//!
//! No date/time crate is used: the `YYYYMMDDTHHMMSSZ` / `YYYYMMDD` clock
//! values are hand-formatted from Unix seconds via civil-date math (Howard
//! Hinnant's `civil_from_days` algorithm).
//!
//! [AWS SigV4 signing process]: https://docs.aws.amazon.com/general/latest/gr/sigv4-signing.html

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::LLMError;

type HmacSha256 = Hmac<Sha256>;

/// AWS credentials used to sign a request.
#[derive(Debug, Clone)]
pub struct AwsCredentials {
    /// AWS access key id (`Credential=` prefix in the authorization header).
    pub access_key_id: String,
    /// AWS secret access key, used to derive the signing key.
    pub secret_access_key: String,
    /// Temporary-credential session token, if any.
    ///
    /// When present, `sign` adds an `x-amz-security-token` header.
    pub session_token: Option<String>,
    /// AWS region the request targets, e.g. `"us-east-1"`.
    pub region: String,
}

impl AwsCredentials {
    /// Resolve credentials from the process environment: `AWS_ACCESS_KEY_ID`
    /// and `AWS_SECRET_ACCESS_KEY` (required), `AWS_SESSION_TOKEN` (optional),
    /// region `AWS_REGION` else `"us-east-1"`. Returns `None` if either
    /// required variable is absent (or empty).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|key| std::env::var(key).ok())
    }

    /// Testable variant of [`AwsCredentials::from_env`]: resolves through the
    /// given `get` lookup instead of `std::env::var`, so callers can exercise
    /// the resolution logic without mutating process env.
    #[must_use]
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
        let access_key_id = non_empty(get("AWS_ACCESS_KEY_ID"))?;
        let secret_access_key = non_empty(get("AWS_SECRET_ACCESS_KEY"))?;
        let session_token = non_empty(get("AWS_SESSION_TOKEN"));
        let region = non_empty(get("AWS_REGION")).unwrap_or_else(|| "us-east-1".to_string());
        Some(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        })
    }
}

/// Inputs describing the request to sign.
#[derive(Debug, Clone, Copy)]
pub struct SignInput<'a> {
    /// HTTP method, e.g. `"POST"`.
    pub method: &'a str,
    /// Full request URL (scheme + host + path; no query string expected for
    /// Bedrock's `converse-stream`).
    pub url: &'a str,
    /// Headers already set on the request. Signed alongside `host`,
    /// `x-amz-date`, and `x-amz-content-sha256`.
    pub headers: &'a BTreeMap<String, String>,
    /// Raw request body, hashed to produce the payload hash.
    pub body: &'a [u8],
    /// AWS service name, e.g. `"bedrock"`.
    pub service: &'a str,
    /// Request timestamp, in Unix seconds. Frozen/injectable for tests.
    pub timestamp_unix_secs: u64,
}

/// Sign `input` with `creds`, returning the headers to add to the request.
///
/// Always returns `authorization`, `x-amz-date`, and `x-amz-content-sha256`.
/// Also returns `x-amz-security-token` when `creds.session_token` is
/// `Some`. The `host` header is derived from `input.url` and included in
/// the signed-headers set, but is not itself returned (the HTTP layer sets
/// it from the URL).
///
/// # Errors
/// Returns [`LLMError::Validation`] if `input.url` cannot be parsed into a
/// host and path.
pub fn sign(
    creds: &AwsCredentials,
    input: &SignInput,
) -> Result<BTreeMap<String, String>, LLMError> {
    let (host, path) = split_url(input.url)?;
    let amz_date = format_amz_date(input.timestamp_unix_secs);
    let date_stamp = datestamp(input.timestamp_unix_secs);
    let payload_hash = sha256_hex(input.body);

    // Canonical headers: caller-provided headers plus the AWS-required
    // ones (host, x-amz-date, x-amz-content-sha256, and x-amz-security-token
    // when using temporary credentials), lowercased, trimmed, and sorted by
    // name (BTreeMap gives us the sort for free). We only trim leading/
    // trailing whitespace here, not collapse internal double-spaces — that
    // stricter normalization never triggers for Bedrock's header values.
    let mut canonical_headers_map: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in input.headers {
        canonical_headers_map.insert(name.to_lowercase(), value.trim().to_string());
    }
    canonical_headers_map.insert("host".to_string(), host);
    canonical_headers_map.insert("x-amz-date".to_string(), amz_date.clone());
    canonical_headers_map.insert("x-amz-content-sha256".to_string(), payload_hash.clone());
    // The session token MUST be signed when present: temporary credentials
    // (assumed roles, SSO, EC2/ECS instance profiles, STS) are the common
    // path for Bedrock, and AWS requires x-amz-security-token to be covered
    // by the signature. Insert it before signed_headers/canonical_headers
    // are derived so it lands in both SignedHeaders and the signature.
    if let Some(token) = &creds.session_token {
        canonical_headers_map.insert("x-amz-security-token".to_string(), token.trim().to_string());
    }

    let signed_headers = canonical_headers_map
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(";");

    let canonical_headers: String = canonical_headers_map
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();

    let canonical_uri = canonical_uri(&path);
    let canonical_query = "";

    let canonical_request = format!(
        "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        input.method
    );

    let scope = format!(
        "{date_stamp}/{}/{}/aws4_request",
        creds.region, input.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        &date_stamp,
    );
    let k_region = hmac_sha256(&k_date, &creds.region);
    let k_service = hmac_sha256(&k_region, input.service);
    let k_signing = hmac_sha256(&k_service, "aws4_request");
    let signature = hex_encode(&hmac_sha256(&k_signing, &string_to_sign));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );

    let mut out = BTreeMap::new();
    out.insert("authorization".to_string(), authorization);
    out.insert("x-amz-date".to_string(), amz_date);
    out.insert("x-amz-content-sha256".to_string(), payload_hash);
    if let Some(token) = &creds.session_token {
        out.insert("x-amz-security-token".to_string(), token.trim().to_string());
    }
    Ok(out)
}

/// Split a URL into its authority (`host[:port]`) and path, dropping any
/// query string or fragment. Hand-rolled: Bedrock URLs are always
/// `scheme://host/path`, so a full URL parser is unnecessary.
fn split_url(url: &str) -> Result<(String, String), LLMError> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| LLMError::Validation(format!("invalid URL (missing scheme): {url}")))?;
    let (authority, path_and_query) = match after_scheme.split_once('/') {
        Some((authority, tail)) => (authority, format!("/{tail}")),
        None => (after_scheme, "/".to_string()),
    };
    let path = path_and_query
        .split('#')
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
    Ok((authority.to_string(), path))
}

/// Percent-encode a URL path for use as a SigV4 canonical URI: every byte
/// outside the RFC 3986 unreserved set (`A-Z a-z 0-9 - _ . ~`) is
/// percent-encoded, while `/` path separators are preserved.
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    path.split('/')
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode a single path segment (no `/` inside).
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `hex(sha256(data))`, lowercase.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

/// `HMAC-SHA256(key, data)`, raw bytes.
fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Lowercase hex encoding.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Convert a day count since the Unix epoch (1970-01-01) into a
/// `(year, month, day)` civil date.
///
/// Port of Howard Hinnant's `civil_from_days` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html>), the standard
/// inverse of "days since epoch". Valid for the full `i64` range; no
/// leap-second handling (matches AWS's own UTC-without-leap-seconds clock).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Split unix seconds into `(year, month, day, hour, minute, second)` UTC
/// civil-date/time components.
fn civil_datetime(unix_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let h = (secs_of_day / 3600) as u32;
    let min = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;
    (y, m, d, h, min, s)
}

/// Format unix seconds as the SigV4 `x-amz-date` value: `YYYYMMDDTHHMMSSZ`.
fn format_amz_date(unix_secs: u64) -> String {
    let (y, m, d, h, min, s) = civil_datetime(unix_secs);
    format!("{y:04}{m:02}{d:02}T{h:02}{min:02}{s:02}Z")
}

/// Format unix seconds as the SigV4 credential-scope date: `YYYYMMDD`.
fn datestamp(unix_secs: u64) -> String {
    let (y, m, d, _, _, _) = civil_datetime(unix_secs);
    format!("{y:04}{m:02}{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            region: "us-east-1".into(),
        }
    }

    fn example_headers() -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "host".to_string(),
            "bedrock-runtime.us-east-1.amazonaws.com".to_string(),
        );
        headers
    }

    #[test]
    fn amz_date_formatting() {
        assert_eq!(format_amz_date(1_700_000_000), "20231114T221320Z");
        assert_eq!(datestamp(1_700_000_000), "20231114");
    }

    #[test]
    fn known_answer_vector() {
        let creds = example_creds();
        let headers = example_headers();
        let input = SignInput {
            method: "POST",
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3/converse-stream",
            headers: &headers,
            body: b"{}",
            service: "bedrock",
            timestamp_unix_secs: 1_700_000_000,
        };
        let out = sign(&creds, &input).unwrap();

        assert_eq!(out.get("x-amz-date").unwrap(), "20231114T221320Z");
        assert!(out.get("authorization").unwrap().starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-1/bedrock/aws4_request"
        ));
        assert!(out.get("authorization").unwrap().contains("SignedHeaders="));
        assert!(out.get("authorization").unwrap().contains("Signature="));
        assert_eq!(out.get("x-amz-content-sha256").unwrap().len(), 64);
        assert!(!out.contains_key("x-amz-security-token"));

        // Regression lock: pinned from a verified-correct run of this exact
        // signer against the frozen inputs above. Any future change to the
        // canonical-request/signing-key construction that changes this
        // value must be treated as a signing bug, not an expected diff.
        assert_eq!(
            out.get("authorization").unwrap(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=6c4f718db5b68b4d14f0280f00b1a6187b0bfab9565db9e5e60974a964f4c07b"
        );
    }

    #[test]
    fn session_token_adds_security_header() {
        let mut creds = example_creds();
        creds.session_token = Some("tok".into());
        let headers = example_headers();
        let input = SignInput {
            method: "POST",
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3/converse-stream",
            headers: &headers,
            body: b"{}",
            service: "bedrock",
            timestamp_unix_secs: 1_700_000_000,
        };
        let out = sign(&creds, &input).unwrap();
        assert_eq!(out.get("x-amz-security-token").unwrap(), "tok");

        // The security token must be SIGNED, not just returned: it must
        // appear in the SignedHeaders list so it's covered by the signature.
        let authorization = out.get("authorization").unwrap();
        assert!(
            authorization.contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
            ),
            "session token not in SignedHeaders: {authorization}"
        );

        // Regression lock for the with-token vector, cross-checked against an
        // independent Python (stdlib hashlib/hmac) re-derivation of the whole
        // pipeline. A change that drops the token from the signed set would
        // change this value and fail here.
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=41a763c13d38cd2a220a5dde5dc663fbd90dc348fb02d6174381d17529a481e2"
        );
    }

    #[test]
    fn canonical_uri_preserves_slashes_and_encodes_colons() {
        assert_eq!(
            canonical_uri("/model/anthropic.claude-3:sonnet/converse-stream"),
            "/model/anthropic.claude-3%3Asonnet/converse-stream"
        );
    }

    #[test]
    fn split_url_extracts_host_and_path() {
        let (host, path) =
            split_url("https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse-stream")
                .unwrap();
        assert_eq!(host, "bedrock-runtime.us-east-1.amazonaws.com");
        assert_eq!(path, "/model/x/converse-stream");
    }

    #[test]
    fn split_url_rejects_missing_scheme() {
        assert!(split_url("not-a-url").is_err());
    }

    #[test]
    fn from_vars_resolves_required_pair_with_default_region() {
        let vars: BTreeMap<&str, &str> = BTreeMap::from([
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "SECRET"),
        ]);
        let creds = AwsCredentials::from_vars(|k| vars.get(k).map(|v| v.to_string()))
            .expect("required pair present");
        assert_eq!(creds.access_key_id, "AKID");
        assert_eq!(creds.secret_access_key, "SECRET");
        assert_eq!(creds.region, "us-east-1");
        assert!(creds.session_token.is_none());
    }

    #[test]
    fn from_vars_reads_session_token_and_region_when_present() {
        let vars: BTreeMap<&str, &str> = BTreeMap::from([
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "SECRET"),
            ("AWS_SESSION_TOKEN", "TOKEN"),
            ("AWS_REGION", "eu-west-1"),
        ]);
        let creds = AwsCredentials::from_vars(|k| vars.get(k).map(|v| v.to_string()))
            .expect("required pair present");
        assert_eq!(creds.session_token.as_deref(), Some("TOKEN"));
        assert_eq!(creds.region, "eu-west-1");
    }

    #[test]
    fn from_vars_none_when_secret_missing() {
        let vars: BTreeMap<&str, &str> = BTreeMap::from([("AWS_ACCESS_KEY_ID", "AKID")]);
        assert!(AwsCredentials::from_vars(|k| vars.get(k).map(|v| v.to_string())).is_none());
    }

    #[test]
    fn from_vars_none_when_access_key_missing() {
        let vars: BTreeMap<&str, &str> = BTreeMap::from([("AWS_SECRET_ACCESS_KEY", "SECRET")]);
        assert!(AwsCredentials::from_vars(|k| vars.get(k).map(|v| v.to_string())).is_none());
    }
}
