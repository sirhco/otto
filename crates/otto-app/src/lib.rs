//! Shared runtime assembly — wires config + auth + providers + tools + MCP +
//! permission + agents + storage into a ready-to-use [`Runtime`] consumed by
//! both the CLI and the server.
//!
//! [`Runtime::load`] performs the full boot: it loads config
//! ([`otto_config::load`]), opens the persistent [`otto_storage::Store`],
//! resolves the agent set, builds the [`otto_permission::Permission`] service
//! from `config.permission`, registers the built-in [`otto_tools`] plus any
//! tools advertised by connected `config.mcp` servers, and installs a
//! [`RouteFactory`] that turns a [`ModelRef`] into a runnable
//! [`otto_llm::Route`] using stored credentials or the environment.
//!
//! [`Runtime::run`] persists the user prompt, assembles the
//! [`otto_session::RunConfig`] (permission gate, tools, subagent spawner, and
//! the live event tap), spawns the agent loop, and hands back a [`RunHandle`]
//! carrying the streaming events and the join handle for the final message.
//!
//! Tests construct a runtime with [`Runtime::in_memory`] and inject a scripted
//! [`RouteFactory`] via [`Runtime::with_route_factory`] so the whole assembly
//! runs headless with no network or disk.

#![forbid(unsafe_code)]

mod route_factory;
mod runtime;
mod title;

pub use route_factory::{AuthRouteFactory, RouteFactory, default_model};
pub use runtime::{RunHandle, Runtime};

/// Result alias for fallible runtime-assembly operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised while assembling or driving a [`Runtime`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A config load/parse failure ([`otto_config`]).
    #[error(transparent)]
    Config(#[from] otto_config::Error),
    /// A credential-store failure ([`otto_auth`]).
    #[error(transparent)]
    Auth(#[from] otto_auth::AuthError),
    /// A persistence failure ([`otto_storage`]).
    #[error(transparent)]
    Storage(#[from] otto_storage::StorageError),
    /// A filesystem failure while preparing the data directory.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A route could not be constructed for the requested model.
    #[error("route error: {0}")]
    Route(String),
}

/// Errors raised by a spawned [`Runtime::run`] task (the [`RunHandle::join`]
/// result).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A persistence failure while seeding the prompt or reading the result.
    #[error(transparent)]
    Storage(#[from] otto_storage::StorageError),
    /// A failure inside the agent loop ([`otto_session::RunError`]).
    #[error(transparent)]
    Session(#[from] otto_session::RunError),
    /// The route factory could not resolve a route for the run's model.
    #[error("route error: {0}")]
    Route(String),
}
