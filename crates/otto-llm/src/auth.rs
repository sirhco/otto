//! Authentication for a route.
//!
//! Port of opencode `packages/llm/src/route/auth.ts`. An [`AuthDef`] resolves
//! secrets (literal or from the environment) and applies them as request
//! headers: `Bearer` for OpenAI-style, `x-api-key` for Anthropic-style.

use std::collections::BTreeMap;

use crate::error::LLMError;

/// A source for a secret value.
///
/// Port of the `secret` / `config(ENV_VAR)` sources in `auth.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// A literal secret value.
    Literal(String),
    /// The name of an environment variable to read (`config(ENV_VAR)`).
    Env(String),
}

impl Secret {
    /// A literal secret.
    #[must_use]
    pub fn literal(value: impl Into<String>) -> Self {
        Secret::Literal(value.into())
    }

    /// A secret read from environment variable `var` (`config(ENV_VAR)`).
    #[must_use]
    pub fn config(var: impl Into<String>) -> Self {
        Secret::Env(var.into())
    }

    /// Resolve the secret to a value, or `None` if an env var is unset/empty.
    #[must_use]
    pub fn resolve(&self) -> Option<String> {
        match self {
            Secret::Literal(v) => Some(v.clone()),
            Secret::Env(k) => std::env::var(k).ok().filter(|v| !v.is_empty()),
        }
    }
}

/// A route's authentication strategy.
///
/// Port of the `Auth` model in `auth.ts`: `none`, `bearer`, `header`,
/// `optional`, and `or_else` chaining, with env fallback via [`Secret::config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDef {
    /// No authentication.
    None,
    /// `authorization: Bearer <secret>` (OpenAI-style).
    Bearer(Secret),
    /// A named header carrying the secret, e.g. `x-api-key`
    /// (Anthropic-style).
    Header {
        /// Header name.
        name: String,
        /// Secret source for the value.
        value: Secret,
    },
    /// Wrap another strategy so a *missing* secret is tolerated (no header).
    Optional(Box<AuthDef>),
    /// Try the first strategy; if its secret is missing, fall back to the
    /// second.
    OrElse(Box<AuthDef>, Box<AuthDef>),
}

impl AuthDef {
    /// No authentication.
    #[must_use]
    pub fn none() -> Self {
        AuthDef::None
    }

    /// `authorization: Bearer <secret>`.
    #[must_use]
    pub fn bearer(secret: Secret) -> Self {
        AuthDef::Bearer(secret)
    }

    /// A named header, e.g. `header("x-api-key", secret)`.
    #[must_use]
    pub fn header(name: impl Into<String>, secret: Secret) -> Self {
        AuthDef::Header {
            name: name.into(),
            value: secret,
        }
    }

    /// Make this strategy optional (a missing secret is not an error).
    #[must_use]
    pub fn optional(self) -> Self {
        AuthDef::Optional(Box::new(self))
    }

    /// Fall back to `other` when this strategy's secret is missing.
    #[must_use]
    pub fn or_else(self, other: AuthDef) -> Self {
        AuthDef::OrElse(Box::new(self), Box::new(other))
    }

    /// Apply this strategy's headers to `headers`.
    ///
    /// # Errors
    /// Returns [`LLMError::Authentication`] with `kind = "missing"` when a
    /// required secret cannot be resolved (unless wrapped in
    /// [`AuthDef::Optional`] / recovered by [`AuthDef::or_else`]).
    pub fn apply(&self, headers: &mut BTreeMap<String, String>) -> Result<(), LLMError> {
        match self {
            AuthDef::None => Ok(()),
            AuthDef::Bearer(secret) => match secret.resolve() {
                Some(value) => {
                    headers.insert("authorization".to_string(), format!("Bearer {value}"));
                    Ok(())
                }
                None => Err(LLMError::auth_missing()),
            },
            AuthDef::Header { name, value } => match value.resolve() {
                Some(v) => {
                    headers.insert(name.clone(), v);
                    Ok(())
                }
                None => Err(LLMError::auth_missing()),
            },
            AuthDef::Optional(inner) => match inner.apply(headers) {
                Ok(()) => Ok(()),
                // A missing secret is tolerated; any other error propagates.
                Err(LLMError::Authentication { .. }) => Ok(()),
                Err(other) => Err(other),
            },
            AuthDef::OrElse(first, second) => match first.apply(headers) {
                Ok(()) => Ok(()),
                Err(LLMError::Authentication { .. }) => second.apply(headers),
                Err(other) => Err(other),
            },
        }
    }
}
