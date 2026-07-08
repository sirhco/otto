//! Per-session runner state — a minimal port of the `state.ensureRunning`
//! seam in opencode `session/prompt.ts` (`prompt.ts:1342-1346`) and the
//! abort plumbing behind `Session.abort`.
//!
//! opencode keeps one running fiber per session and interrupts it on abort.
//! Here a [`RunnerRegistry`] holds one [`CancellationToken`] per session id;
//! [`RunnerRegistry::cancel`] triggers the token, which the
//! [`crate::processor::Processor`] cleanup observes to mark running tools
//! interrupted and finalize the assistant message.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

/// A registry of per-session cancellation tokens.
///
/// One runner per session: [`start`](RunnerRegistry::start) registers (and
/// replaces) the token for a session, [`cancel`](RunnerRegistry::cancel) fires
/// it, and [`finish`](RunnerRegistry::finish) drops it once the run ends.
#[derive(Default)]
pub struct RunnerRegistry {
    runners: Mutex<HashMap<String, CancellationToken>>,
}

impl RunnerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
        }
    }

    /// Register a fresh [`CancellationToken`] for `session_id`, replacing (and
    /// cancelling) any prior runner, and return the new token to pass into
    /// [`crate::run::run_loop`].
    pub fn start(&self, session_id: impl Into<String>) -> CancellationToken {
        let token = CancellationToken::new();
        let mut runners = self.runners.lock().expect("runner registry poisoned");
        if let Some(prev) = runners.insert(session_id.into(), token.clone()) {
            prev.cancel();
        }
        token
    }

    /// Cancel the runner for `session_id`, if one is registered.
    pub fn cancel(&self, session_id: &str) {
        if let Some(token) = self
            .runners
            .lock()
            .expect("runner registry poisoned")
            .get(session_id)
        {
            token.cancel();
        }
    }

    /// Whether a runner is registered for `session_id`.
    #[must_use]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.runners
            .lock()
            .expect("runner registry poisoned")
            .contains_key(session_id)
    }

    /// Drop the runner for `session_id` once its run has ended.
    pub fn finish(&self, session_id: &str) {
        self.runners
            .lock()
            .expect("runner registry poisoned")
            .remove(session_id);
    }
}
