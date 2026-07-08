//! Credential store (`auth.json`, mode `0600`) + provider OAuth flows.
//!
//! Port of opencode `packages/opencode/src/auth/index.ts` (the credential
//! store) and the built-in auth plugins under
//! `packages/opencode/src/plugin/` (`openai/codex.ts`,
//! `github-copilot/copilot.ts`), plus the standard Anthropic Claude Pro/Max
//! OAuth flow (which is *not* in the source repo — see
//! [`providers::anthropic`] for the `TODO(confirm)` endpoints/client id).
//!
//! # Layout
//! - [`credential::Credential`] — the `Oauth | Api | WellKnown` union.
//! - [`store::AuthStore`] — read/write `auth.json`, `0600`, slash-normalised.
//! - [`pkce::Pkce`] — PKCE verifier/S256 challenge.
//! - [`providers`] — anthropic / api_key / copilot flows + [`providers::resolve`].

#![forbid(unsafe_code)]

pub mod credential;
pub mod error;
pub mod pkce;
pub mod providers;
pub mod store;
mod util;

pub use credential::Credential;
pub use error::{AuthError, Result};
pub use pkce::{Pkce, challenge_for};
pub use providers::{
    DEFAULT_EXPIRY_MARGIN_MS, ResolvedCredential, Resolver, anthropic, api_key, copilot, resolve,
};
pub use store::{AUTH_CONTENT_ENV, AuthMap, AuthStore, OAUTH_DUMMY_KEY};
