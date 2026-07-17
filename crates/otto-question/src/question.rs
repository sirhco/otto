//! The [`Question`] service: `ask`/`reply`/`list_pending`/`subscribe` over a
//! pending-request registry, with no ruleset/mode/policy dimension.

use std::collections::HashMap;
use std::sync::Mutex;

use otto_tools::tool::{QuestionOutcome, QuestionPrompt};
use tokio::sync::{broadcast, oneshot};

/// A question-request identifier (`que_…`, from [`otto_id`]).
pub use otto_id::QuestionId as RequestId;
/// Session identifier — the same type otto-storage uses for its `Session.id`.
pub use otto_id::SessionId;

/// Event emitted when a request starts blocking on a human decision.
#[derive(Debug, Clone)]
pub struct Asked {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub questions: Vec<QuestionPrompt>,
}

/// A snapshot of one pending request — the return element of
/// [`Question::list_pending`].
#[derive(Debug, Clone)]
pub struct PendingInfo {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub questions: Vec<QuestionPrompt>,
}

/// One in-flight request, held until a reply resolves its `responder`.
struct Pending {
    session_id: SessionId,
    questions: Vec<QuestionPrompt>,
    responder: oneshot::Sender<QuestionOutcome>,
}

struct Inner {
    pending: HashMap<RequestId, Pending>,
}

/// The question service.
pub struct Question {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Asked>,
}

impl Default for Question {
    fn default() -> Self {
        Self::new()
    }
}

impl Question {
    /// Create a service with no pending requests.
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Mutex::new(Inner {
                pending: HashMap::new(),
            }),
            events,
        }
    }

    /// Subscribe to [`Asked`] events. A server/CLI drives the prompt UI from
    /// this stream and calls [`reply`](Question::reply) with the answer.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Asked> {
        self.events.subscribe()
    }

    /// Ask the user to answer `questions` on behalf of `session_id`. Always
    /// suspends — unlike `otto_permission::Permission::ask`, there is no
    /// deny-gate or interactivity-mode check that can return without
    /// registering a pending request.
    ///
    /// Registration (the pending-map insert and the [`Asked`] publish) runs
    /// eagerly, synchronously, when this is called — not lazily on first
    /// poll — so the returned future owns its receiver outright and carries
    /// no borrow of `self`. That is what lets a pending ask outlive the
    /// [`Question`] it was registered against: dropping the service drops
    /// the responder, and the still-running future observes the closed
    /// channel as [`QuestionOutcome::Cancelled`].
    pub fn ask(
        &self,
        session_id: impl Into<SessionId>,
        questions: Vec<QuestionPrompt>,
    ) -> impl std::future::Future<Output = QuestionOutcome> + Send + 'static {
        let session_id = session_id.into();
        let receiver = {
            let mut inner = self.inner.lock().expect("question mutex poisoned");
            let request_id = RequestId::new_ascending();
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(
                request_id.clone(),
                Pending {
                    session_id: session_id.clone(),
                    questions: questions.clone(),
                    responder: tx,
                },
            );
            let _ = self.events.send(Asked {
                request_id,
                session_id,
                questions,
            });
            rx
        };
        async move { receiver.await.unwrap_or(QuestionOutcome::Cancelled) }
    }

    /// Resolve a pending request. Returns `true` if `request_id` matched a
    /// pending request.
    pub fn reply(&self, request_id: &str, outcome: QuestionOutcome) -> bool {
        let mut inner = self.inner.lock().expect("question mutex poisoned");
        let Some(existing) = inner.pending.remove(request_id) else {
            return false;
        };
        let _ = existing.responder.send(outcome);
        true
    }

    /// Snapshot the currently pending requests.
    #[must_use]
    pub fn list_pending(&self) -> Vec<PendingInfo> {
        let inner = self.inner.lock().expect("question mutex poisoned");
        inner
            .pending
            .iter()
            .map(|(id, p)| PendingInfo {
                request_id: id.clone(),
                session_id: p.session_id.clone(),
                questions: p.questions.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_tools::tool::QuestionOption;

    fn one_question() -> QuestionPrompt {
        QuestionPrompt {
            question: "Pick one".into(),
            header: "choice".into(),
            options: vec![QuestionOption {
                label: "A".into(),
                description: "first".into(),
            }],
            multiple: false,
        }
    }

    #[tokio::test]
    async fn ask_blocks_until_reply() {
        let q = std::sync::Arc::new(Question::new());
        let mut rx = q.subscribe();
        let q2 = q.clone();
        let h = tokio::spawn(async move { q2.ask("ses_1", vec![one_question()]).await });
        let asked = rx.recv().await.expect("ask publishes an Asked event");
        assert_eq!(asked.session_id, "ses_1");
        assert_eq!(asked.questions.len(), 1);
        q.reply(&asked.request_id, QuestionOutcome::Answered(vec![vec![0]]));
        let outcome = h.await.unwrap();
        assert_eq!(outcome, QuestionOutcome::Answered(vec![vec![0]]));
    }

    #[tokio::test]
    async fn reply_cancelled_resolves_the_ask() {
        let q = std::sync::Arc::new(Question::new());
        let mut rx = q.subscribe();
        let q2 = q.clone();
        let h = tokio::spawn(async move { q2.ask("ses_1", vec![one_question()]).await });
        let asked = rx.recv().await.expect("ask publishes an Asked event");
        q.reply(&asked.request_id, QuestionOutcome::Cancelled);
        assert_eq!(h.await.unwrap(), QuestionOutcome::Cancelled);
    }

    #[test]
    fn reply_to_unknown_id_returns_false() {
        let q = Question::new();
        assert!(!q.reply("que_bogus", QuestionOutcome::Cancelled));
    }

    #[tokio::test]
    async fn dropping_the_service_cancels_pending_asks() {
        let q = std::sync::Arc::new(Question::new());
        let mut rx = q.subscribe();
        let q2 = q.clone();
        let h = tokio::spawn(async move { q2.ask("ses_1", vec![one_question()]).await });
        let _asked = rx.recv().await.expect("ask publishes an Asked event");
        drop(q); // last strong ref besides the one moved into the task
        // The task's own clone keeps the service alive until it returns, so
        // dropping the outer `q` alone doesn't tear down `Inner` — instead,
        // assert the pending entry is never replied and the task hangs
        // until explicitly resolved. Re-derive: use a fresh service with no
        // extra clone to prove teardown really does cancel.
        h.abort();
        let _ = h.await;

        let q3 = Question::new();
        let mut rx3 = q3.subscribe();
        // `ask` registers the pending request synchronously (before this
        // call returns), so the resulting future carries no borrow of `q3`
        // — dropping `q3` out from under it is exactly what this test needs
        // to exercise.
        let ask_fut = q3.ask("ses_1", vec![one_question()]);
        let _asked3 = rx3.recv().await.expect("ask publishes an Asked event");
        drop(q3);
        let outcome = ask_fut.await;
        assert_eq!(outcome, QuestionOutcome::Cancelled);
    }

    #[tokio::test]
    async fn list_pending_snapshots_in_flight_requests() {
        let q = std::sync::Arc::new(Question::new());
        let mut rx = q.subscribe();
        let q2 = q.clone();
        let h = tokio::spawn(async move { q2.ask("ses_1", vec![one_question()]).await });
        let asked = rx.recv().await.expect("ask publishes an Asked event");
        let pending = q.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, asked.request_id);
        q.reply(&asked.request_id, QuestionOutcome::Cancelled);
        h.await.unwrap();
        assert!(q.list_pending().is_empty());
    }
}
