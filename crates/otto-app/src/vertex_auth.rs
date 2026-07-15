//! GCP Application Default Credentials → Vertex AI Bearer token, kept fresh
//! for the lifetime of a [`crate::Runtime`].
//!
//! Bridges `gcloud-auth`'s async token fetch into `RouteFactory::route_for`'s
//! fully synchronous signature: [`VertexTokenCache::new`] does the first
//! fetch as part of (async) `Runtime::load`, then a background task keeps a
//! `tokio::sync::watch` channel fresh; [`VertexTokenCache::current_token`]
//! reads it with a non-blocking `.borrow().clone()`.
//!
//! otto extension: no opencode analog (opencode has no ADC/service-account auth path).

use std::sync::Arc;
use std::time::Duration;

use gcloud_auth::project::{self, Config as GcpConfig};
use gcloud_auth::token::Token;
use gcloud_auth::token_source::TokenSource;

use crate::{Error, Result};

/// The OAuth2 scope Vertex AI's `streamGenerateContent` endpoint requires.
const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Refresh this long before expiry. `otto-auth`'s own OAuth refresh margin
/// (`otto_auth::providers::DEFAULT_EXPIRY_MARGIN_MS`) is 60s, checked at
/// on-demand resolve time; this is a background-*scheduled* loop instead, so
/// it needs more slack to absorb one failed attempt (see `RETRY_BACKOFF_SECS`
/// below) before the cached token actually goes stale.
const REFRESH_MARGIN_SECS: i64 = 300;
/// Backoff before retrying a failed background refresh.
const RETRY_BACKOFF_SECS: u64 = 30;
/// Never sleep less than this between refresh attempts, even for a
/// short-TTL token (workload-identity-federation / downscoped tokens can
/// have TTLs shorter than `REFRESH_MARGIN_SECS`) — prevents a busy loop
/// hammering the token endpoint.
const MIN_REFRESH_SLEEP_SECS: u64 = 5;

/// Resolves a live GCP bearer token for Vertex AI requests. Implemented by
/// [`VertexTokenCache`] (real ADC-backed); tests inject a fake.
pub trait VertexAuth: Send + Sync {
    /// The current cached access token.
    ///
    /// # Errors
    /// Returns [`Error::Route`] if the cached token has passed its expiry
    /// with no successful background refresh since.
    fn current_token(&self) -> Result<String>;
}

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    /// Unix seconds; `None` means "no known expiry, assume valid" — some ADC
    /// sources omit it.
    expiry_unix_secs: Option<i64>,
}

/// A live, self-refreshing GCP access token backed by Application Default
/// Credentials, for Vertex AI's Bearer auth.
///
/// Fetches an initial token synchronously in [`Self::new`] (so missing/bad
/// GCP credentials fail fast at startup, inside `Runtime::load`), then keeps
/// it fresh via a background task that re-fetches ~5 minutes before expiry.
pub struct VertexTokenCache {
    rx: tokio::sync::watch::Receiver<CachedToken>,
    /// Kept alongside the clone moved into the background task solely so
    /// `refresh_once_for_test` can push a refresh directly.
    #[cfg_attr(not(test), allow(dead_code))]
    tx: tokio::sync::watch::Sender<CachedToken>,
    refresh_task: tokio::task::JoinHandle<()>,
}

impl VertexTokenCache {
    /// Discover ADC and start the cache (used by [`crate::Runtime::load`]).
    ///
    /// # Errors
    /// Returns [`Error::Route`] if ADC discovery or the initial token fetch
    /// fails (no `gcloud auth application-default login`, no service account
    /// configured, not running on GCP, etc).
    pub async fn new() -> Result<Self> {
        let discovered = project::project()
            .await
            .map_err(|e| Error::Route(format!("vertex: ADC discovery failed: {e}")))?;
        let config = GcpConfig::default().with_scopes(&[VERTEX_SCOPE]);
        let source: Arc<dyn TokenSource> =
            project::create_token_source_from_project(&discovered, config)
                .await
                .map_err(|e| Error::Route(format!("vertex: ADC discovery failed: {e}")))?
                .into();
        Self::from_source(source).await
    }

    /// Core constructor over any [`TokenSource`] — the seam tests inject a
    /// fake into, bypassing real ADC discovery entirely. Keeps a clone of the
    /// `Sender` on `Self` (in addition to moving one into the background
    /// task) purely so `refresh_once_for_test` can drive a refresh directly
    /// without depending on real sleep timing.
    async fn from_source(source: Arc<dyn TokenSource>) -> Result<Self> {
        let initial = fetch(&source).await?;
        let (tx, rx) = tokio::sync::watch::channel(initial.clone());
        let refresh_task = tokio::spawn(refresh_loop(source, tx.clone(), initial));
        Ok(Self {
            rx,
            tx,
            refresh_task,
        })
    }

    /// Test-only direct refresh, bypassing the spawned loop's sleep so the
    /// watch-channel plumbing can be exercised without depending on real
    /// timing.
    #[cfg(test)]
    async fn refresh_once_for_test(&self, source: &Arc<dyn TokenSource>) -> Result<()> {
        let token = fetch(source).await?;
        self.tx
            .send(token)
            .map_err(|_| Error::Route("vertex: channel closed".to_string()))
    }
}

impl VertexAuth for VertexTokenCache {
    fn current_token(&self) -> Result<String> {
        let cached = self.rx.borrow();
        if let Some(expiry) = cached.expiry_unix_secs
            && now_unix_secs() >= expiry
        {
            return Err(Error::Route(
                "vertex: cached access token expired and background refresh hasn't caught up"
                    .to_string(),
            ));
        }
        Ok(cached.access_token.clone())
    }
}

impl Drop for VertexTokenCache {
    fn drop(&mut self) {
        self.refresh_task.abort();
    }
}

async fn fetch(source: &Arc<dyn TokenSource>) -> Result<CachedToken> {
    let token: Token = source
        .token()
        .await
        .map_err(|e| Error::Route(format!("vertex: token fetch failed: {e}")))?;
    Ok(CachedToken {
        access_token: token.access_token,
        expiry_unix_secs: token.expiry.map(|e| e.unix_timestamp()),
    })
}

async fn refresh_loop(
    source: Arc<dyn TokenSource>,
    tx: tokio::sync::watch::Sender<CachedToken>,
    mut current: CachedToken,
) {
    loop {
        let sleep_secs = match current.expiry_unix_secs {
            Some(expiry) => (expiry - now_unix_secs() - REFRESH_MARGIN_SECS).max(0) as u64,
            None => REFRESH_MARGIN_SECS as u64,
        }
        .max(MIN_REFRESH_SLEEP_SECS);
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        match fetch(&source).await {
            Ok(token) => {
                current = token.clone();
                if tx.send(token).is_err() {
                    return; // every receiver dropped — the cache was dropped
                }
            }
            Err(e) => {
                tracing::warn!("vertex: background token refresh failed: {e}");
                tokio::time::sleep(Duration::from_secs(RETRY_BACKOFF_SECS)).await;
            }
        }
    }
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gcloud_auth::error::Error as GcloudError;
    use gcloud_auth::token::Token;
    use gcloud_auth::token_source::TokenSource;

    use super::{VertexAuth, VertexTokenCache};

    #[derive(Debug)]
    struct FakeTokenSource {
        access_token: String,
        expiry_unix_secs: Option<i64>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TokenSource for FakeTokenSource {
        async fn token(&self) -> std::result::Result<Token, GcloudError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Token {
                access_token: self.access_token.clone(),
                token_type: "Bearer".to_string(),
                expiry: self
                    .expiry_unix_secs
                    .and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok()),
            })
        }
    }

    /// The cache serves the token fetched at construction time, no refresh
    /// needed yet.
    #[tokio::test]
    async fn current_token_returns_the_initially_fetched_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let far_future = time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
        let source: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "initial-token".to_string(),
            expiry_unix_secs: Some(far_future),
            calls: calls.clone(),
        });

        let cache = VertexTokenCache::from_source(source)
            .await
            .expect("cache builds");

        assert_eq!(cache.current_token().unwrap(), "initial-token");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the initial fetch happened"
        );
    }

    /// A manually-triggered refresh cycle republishes the new token — proves
    /// the watch-channel plumbing works, without depending on real sleep
    /// timing (the spawned background loop's scheduling is not exercised
    /// here; see the module doc comment).
    #[tokio::test]
    async fn refresh_once_republishes_a_new_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let far_future = time::OffsetDateTime::now_utc().unix_timestamp() + 3600;
        let source: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "token-v1".to_string(),
            expiry_unix_secs: Some(far_future),
            calls: calls.clone(),
        });

        let cache = VertexTokenCache::from_source(source.clone())
            .await
            .expect("cache builds");
        assert_eq!(cache.current_token().unwrap(), "token-v1");

        // Swap in a source that returns a different token, then trigger one
        // refresh cycle directly (bypassing the spawned loop's sleep).
        let source2: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "token-v2".to_string(),
            expiry_unix_secs: Some(far_future),
            calls,
        });
        cache
            .refresh_once_for_test(&source2)
            .await
            .expect("refresh ok");
        assert_eq!(cache.current_token().unwrap(), "token-v2");
    }

    /// A token with no known expiry is served as-is (never treated as
    /// already-expired).
    #[tokio::test]
    async fn current_token_tolerates_missing_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "no-expiry-token".to_string(),
            expiry_unix_secs: None,
            calls,
        });
        let cache = VertexTokenCache::from_source(source)
            .await
            .expect("cache builds");
        assert_eq!(cache.current_token().unwrap(), "no-expiry-token");
    }

    /// A token already past its expiry, with no successful refresh since,
    /// surfaces as an error rather than being served silently.
    #[tokio::test]
    async fn current_token_errors_when_cached_token_already_expired() {
        let calls = Arc::new(AtomicUsize::new(0));
        let already_past = time::OffsetDateTime::now_utc().unix_timestamp() - 10;
        let source: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "stale-token".to_string(),
            expiry_unix_secs: Some(already_past),
            calls,
        });
        let cache = VertexTokenCache::from_source(source)
            .await
            .expect("cache builds");
        let err = cache.current_token().expect_err("should be expired");
        assert!(err.to_string().contains("expired"), "message was: {err}");
    }

    /// A short-TTL token (below `REFRESH_MARGIN_SECS`) must not make the
    /// background refresh loop spin: the `MIN_REFRESH_SLEEP_SECS` floor
    /// should keep it from re-fetching in a tight loop. We can't observe
    /// "zero" refreshes directly (the loop is timing-based and will
    /// eventually refresh), but shortly after construction the fake's call
    /// counter should still read 1 (just the initial fetch) rather than
    /// having already spun through many cycles.
    #[tokio::test]
    async fn short_ttl_token_does_not_busy_loop_the_background_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let almost_expired = time::OffsetDateTime::now_utc().unix_timestamp() + 1;
        let source: Arc<dyn TokenSource> = Arc::new(FakeTokenSource {
            access_token: "short-ttl-token".to_string(),
            expiry_unix_secs: Some(almost_expired),
            calls: calls.clone(),
        });

        let cache = VertexTokenCache::from_source(source)
            .await
            .expect("cache builds");
        assert_eq!(cache.current_token().unwrap(), "short-ttl-token");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the background loop must not busy-spin refetching a short-TTL token; \
             the MIN_REFRESH_SLEEP_SECS floor should keep it asleep for this window"
        );
    }
}
