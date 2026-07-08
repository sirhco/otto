//! The [`otto_tools::PermissionGate`] adapter that the session loop injects
//! into `ToolContext`.

use std::sync::Arc;

use async_trait::async_trait;
use otto_tools::{PermissionDenied, PermissionGate, PermissionRequest};

use crate::permission::{Permission, SessionId};

/// A [`PermissionGate`] bound to one session that delegates to a shared
/// [`Permission`] service.
///
/// opencode threads `ctx.ask(...)` (the session's permission closure) into
/// every mutating tool; this is the Rust seam that carries the `sessionID` so
/// `Tool::execute` can call `ctx.permission.ask(req)` without knowing which
/// session it runs in.
#[derive(Clone)]
pub struct SessionGate {
    permission: Arc<Permission>,
    session_id: SessionId,
}

impl SessionGate {
    /// Bind `permission` to `session_id`.
    #[must_use]
    pub fn new(permission: Arc<Permission>, session_id: impl Into<SessionId>) -> Self {
        Self {
            permission,
            session_id: session_id.into(),
        }
    }
}

#[async_trait]
impl PermissionGate for SessionGate {
    async fn ask(&self, req: PermissionRequest) -> Result<(), PermissionDenied> {
        self.permission.ask(self.session_id.clone(), req).await
    }
}
