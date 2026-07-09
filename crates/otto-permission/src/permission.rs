//! The interactive ask/reply permission gate — a port of the `Service`
//! (`ask` ~67, `reply` ~109, `list` ~169) in opencode `permission/index.ts`.
//!
//! [`Permission`] holds the configured [`Ruleset`], a per-session set of
//! runtime-approved rules, and the set of in-flight (pending) requests. A
//! caller [`ask`](Permission::ask)s for permission; if any pattern requires a
//! prompt the call registers a pending request, publishes an [`Asked`] event,
//! and blocks until a UI/CLI [`reply`](Permission::reply)s.

use std::collections::HashMap;
use std::sync::Mutex;

use otto_id::{Prefix, ascending};
use otto_tools::{PermissionDenied, PermissionRequest};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::mode::{PermissionMode, danger_ruleset, mode_overlay};
use crate::ruleset::{Action, Rule, Ruleset, evaluate};

/// Session identifier (matches otto session id strings).
pub type SessionId = String;
/// Permission-request identifier (`per_…`, from [`otto_id`]).
pub type RequestId = String;

/// The user's answer to a pending request — port of `PermissionV1.Reply`
/// (`v1/permission.ts`, `["once", "always", "reject"]`) plus the optional
/// correction `message` carried by `ReplyBody`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Approve this call only.
    Once,
    /// Approve and remember the request's `always` patterns for the session.
    Always,
    /// Reject the call, optionally with feedback for the model.
    Reject {
        /// Correction text surfaced to the model (opencode `CorrectedError`).
        message: Option<String>,
    },
}

/// Event emitted when a request starts blocking on a human decision — port of
/// `Permission.Event.Asked` (`index.ts:100`, `permission.asked`).
#[derive(Debug, Clone)]
pub struct Asked {
    /// The generated request id.
    pub request_id: RequestId,
    /// The owning session.
    pub session_id: SessionId,
    /// The permission being requested.
    pub permission: String,
    /// The concrete patterns this call touches.
    pub patterns: Vec<String>,
    /// Free-form request metadata (diff, filepath, command, …).
    pub metadata: Value,
}

/// A snapshot of one pending request — the return element of
/// [`Permission::list_pending`], mirroring `PermissionV1.Request`
/// (`index.ts:169` `list`).
#[derive(Debug, Clone)]
pub struct PendingInfo {
    /// The request id.
    pub request_id: RequestId,
    /// The owning session.
    pub session_id: SessionId,
    /// The permission being requested.
    pub permission: String,
    /// The concrete patterns this call touches.
    pub patterns: Vec<String>,
    /// Free-form request metadata.
    pub metadata: Value,
}

/// Internal resolution delivered to a blocked `ask` over its oneshot.
enum Outcome {
    /// The request was approved (once, always, or auto-resolved).
    Approved,
    /// The request was rejected.
    Rejected,
}

/// One in-flight request, held until a reply resolves its `responder`.
struct Pending {
    session_id: SessionId,
    permission: String,
    patterns: Vec<String>,
    always: Vec<String>,
    metadata: Value,
    responder: oneshot::Sender<Outcome>,
}

/// Mutable state guarded by a single mutex.
struct Inner {
    /// Runtime approvals granted via `Reply::Always`, keyed by session.
    approved: HashMap<SessionId, Vec<Rule>>,
    /// In-flight requests awaiting a reply.
    pending: HashMap<RequestId, Pending>,
    /// Per-session permission mode; absent → walk `parents`, then
    /// `default_mode`.
    modes: HashMap<SessionId, PermissionMode>,
    /// Child → parent session links, so a child session (subagent, workflow)
    /// with no explicit mode inherits the nearest ancestor's mode live.
    parents: HashMap<SessionId, SessionId>,
}

/// Upper bound on the parent-chain walk — cheap insurance alongside the
/// visited-set cycle guard.
const PARENT_CHAIN_MAX_DEPTH: usize = 64;

/// Resolve the mode for `session_id`: its own entry, else the nearest
/// ancestor's entry via `parents`, else `default_mode`. An explicit
/// `set_mode` on a child always shadows the parent.
fn resolve_mode(inner: &Inner, session_id: &str, default_mode: PermissionMode) -> PermissionMode {
    let mut current = session_id;
    let mut seen: Vec<&str> = Vec::new();
    for _ in 0..PARENT_CHAIN_MAX_DEPTH {
        if let Some(mode) = inner.modes.get(current) {
            return *mode;
        }
        seen.push(current);
        match inner.parents.get(current) {
            Some(parent) if !seen.contains(&parent.as_str()) => current = parent,
            _ => break,
        }
    }
    default_mode
}

/// The permission service — port of the `Service` in `permission/index.ts`.
pub struct Permission {
    ruleset: Ruleset,
    default_mode: PermissionMode,
    inner: Mutex<Inner>,
    events: broadcast::Sender<Asked>,
}

impl Permission {
    /// Create a service configured with `ruleset`, defaulting every session to
    /// `PermissionMode::default()` (approve-each).
    #[must_use]
    pub fn new(ruleset: Ruleset) -> Self {
        Self::with_mode(ruleset, PermissionMode::default())
    }

    /// Create a service with an explicit per-session default mode.
    #[must_use]
    pub fn with_mode(ruleset: Ruleset, default_mode: PermissionMode) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            ruleset,
            default_mode,
            inner: Mutex::new(Inner {
                approved: HashMap::new(),
                pending: HashMap::new(),
                modes: HashMap::new(),
                parents: HashMap::new(),
            }),
            events,
        }
    }

    /// The current mode for `session_id` — its own mode, else the nearest
    /// ancestor's (via [`link_parent`](Permission::link_parent)), else the
    /// default.
    #[must_use]
    pub fn mode(&self, session_id: &str) -> PermissionMode {
        let inner = self.inner.lock().expect("permission mutex poisoned");
        resolve_mode(&inner, session_id, self.default_mode)
    }

    /// Set the mode for `session_id` (live; affects subsequent `ask` calls).
    pub fn set_mode(&self, session_id: impl Into<SessionId>, mode: PermissionMode) {
        let mut inner = self.inner.lock().expect("permission mutex poisoned");
        inner.modes.insert(session_id.into(), mode);
    }

    /// Record `child`'s parent session so mode resolution can walk the chain.
    /// Inheritance is live: flipping the parent's mode changes what every
    /// linked descendant resolves on its *next* ask, while an explicit
    /// `set_mode` on the child still shadows the parent.
    pub fn link_parent(&self, child: impl Into<SessionId>, parent: impl Into<SessionId>) {
        let mut inner = self.inner.lock().expect("permission mutex poisoned");
        inner.parents.insert(child.into(), parent.into());
    }

    /// Subscribe to [`Asked`] events. A server/CLI drives the prompt UI from
    /// this stream and calls [`reply`](Permission::reply) with the answer.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Asked> {
        self.events.subscribe()
    }

    /// Ask for permission on behalf of `session_id` — port of `ask`
    /// (`index.ts:67`).
    ///
    /// Each pattern is evaluated against `[mode overlay, configured ruleset,
    /// session approvals, danger ruleset]`. Any `Deny` rejects immediately; if
    /// every pattern is `Allow` the call returns `Ok`; otherwise (at least one
    /// `Ask`) a pending request is registered, an [`Asked`] event is
    /// published, and the call blocks on a oneshot until
    /// [`reply`](Permission::reply) resolves it.
    pub async fn ask(
        &self,
        session_id: impl Into<SessionId>,
        req: PermissionRequest,
    ) -> Result<(), PermissionDenied> {
        let session_id = session_id.into();

        let receiver = {
            let mut inner = self.inner.lock().expect("permission mutex poisoned");
            let session_approved =
                Ruleset(inner.approved.get(&session_id).cloned().unwrap_or_default());
            let mode = resolve_mode(&inner, &session_id, self.default_mode);
            let overlay = mode_overlay(mode);
            let danger = danger_ruleset();

            let mut needs_ask = false;
            for pattern in &req.patterns {
                let resolved = evaluate(
                    &[&overlay, &self.ruleset, &session_approved, &danger],
                    &req.permission,
                    pattern,
                );
                match resolved.action {
                    Action::Deny => {
                        return Err(PermissionDenied {
                            permission: req.permission.clone(),
                        });
                    }
                    Action::Allow => {}
                    Action::Ask => needs_ask = true,
                }
            }

            if !needs_ask {
                return Ok(());
            }

            let request_id = ascending(Prefix::Permission);
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(
                request_id.clone(),
                Pending {
                    session_id: session_id.clone(),
                    permission: req.permission.clone(),
                    patterns: req.patterns.clone(),
                    always: req.always.clone(),
                    metadata: req.metadata.clone(),
                    responder: tx,
                },
            );

            // Publish while holding the lock is fine (broadcast::send is sync
            // and non-blocking); subscribers observe a consistent pending set.
            let _ = self.events.send(Asked {
                request_id,
                session_id: session_id.clone(),
                permission: req.permission.clone(),
                patterns: req.patterns.clone(),
                metadata: req.metadata.clone(),
            });

            rx
        };

        match receiver.await {
            Ok(Outcome::Approved) => Ok(()),
            // Rejected, or the service dropped (finalizer semantics from
            // `index.ts:57` — pending requests fail on teardown).
            Ok(Outcome::Rejected) | Err(_) => Err(PermissionDenied {
                permission: req.permission,
            }),
        }
    }

    /// Resolve a pending request — port of `reply` (`index.ts:109`).
    ///
    /// * `Reply::Reject` fails the target request and **cascades** the
    ///   rejection to every other pending request in the same session
    ///   (`index.ts:129`).
    /// * `Reply::Once` approves only the target.
    /// * `Reply::Always` approves the target, records its `always` patterns as
    ///   session approvals, then **auto-resolves** any other pending request in
    ///   the session whose patterns are now all `Allow` (`index.ts:153`).
    ///
    /// Returns `true` if `request_id` matched a pending request.
    pub fn reply(&self, request_id: &str, reply: Reply) -> bool {
        let mut inner = self.inner.lock().expect("permission mutex poisoned");

        let Some(existing) = inner.pending.remove(request_id) else {
            return false;
        };

        match reply {
            Reply::Reject { message } => {
                // The correction `message` (opencode's `CorrectedError` feedback,
                // `index.ts:125`) has no home on the tool seam's `PermissionDenied`
                // — which carries only `permission` — so it is dropped here. A
                // richer denial error can thread it once the seam grows a field.
                let _ = message;
                let _ = existing.responder.send(Outcome::Rejected);
                // Cascade reject the rest of the session.
                let session = existing.session_id.clone();
                let cascade: Vec<RequestId> = inner
                    .pending
                    .iter()
                    .filter(|(_, p)| p.session_id == session)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in cascade {
                    if let Some(p) = inner.pending.remove(&id) {
                        let _ = p.responder.send(Outcome::Rejected);
                    }
                }
            }
            Reply::Once => {
                let _ = existing.responder.send(Outcome::Approved);
            }
            Reply::Always => {
                let session = existing.session_id.clone();
                // Grant the request's `always` patterns to the session.
                let bucket = inner.approved.entry(session.clone()).or_default();
                for pattern in &existing.always {
                    bucket.push(Rule {
                        permission: existing.permission.clone(),
                        pattern: pattern.clone(),
                        action: Action::Allow,
                    });
                }
                let _ = existing.responder.send(Outcome::Approved);

                // Auto-resolve other pending requests now satisfied by the
                // session's approvals (evaluated against approvals only, per
                // `index.ts:156`).
                let approved = Ruleset(inner.approved.get(&session).cloned().unwrap_or_default());
                let satisfied: Vec<RequestId> = inner
                    .pending
                    .iter()
                    .filter(|(_, p)| p.session_id == session)
                    .filter(|(_, p)| {
                        p.patterns.iter().all(|pat| {
                            evaluate(&[&approved], &p.permission, pat).action == Action::Allow
                        })
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in satisfied {
                    if let Some(p) = inner.pending.remove(&id) {
                        let _ = p.responder.send(Outcome::Approved);
                    }
                }
            }
        }

        true
    }

    /// Snapshot the currently pending requests — port of `list`
    /// (`index.ts:169`).
    #[must_use]
    pub fn list_pending(&self) -> Vec<PendingInfo> {
        let inner = self.inner.lock().expect("permission mutex poisoned");
        inner
            .pending
            .iter()
            .map(|(id, p)| PendingInfo {
                request_id: id.clone(),
                session_id: p.session_id.clone(),
                permission: p.permission.clone(),
                patterns: p.patterns.clone(),
                metadata: p.metadata.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::PermissionMode;

    #[tokio::test]
    async fn full_auto_auto_allows_normal_command() {
        let perm = Permission::with_mode(Ruleset::new(), PermissionMode::ApproveEach);
        perm.set_mode("ses_1", PermissionMode::FullAuto);
        let req = PermissionRequest {
            permission: "bash".into(),
            patterns: vec!["cargo test".into()],
            always: vec![],
            metadata: serde_json::json!({}),
        };
        // Auto-allowed: returns Ok without any reply being sent.
        perm.ask("ses_1", req)
            .await
            .expect("full-auto allows normal command");
    }

    #[tokio::test]
    async fn full_auto_still_asks_on_danger() {
        let perm = Permission::with_mode(Ruleset::new(), PermissionMode::FullAuto);
        let mut rx = perm.subscribe();
        let req = PermissionRequest {
            permission: "bash".into(),
            patterns: vec!["rm -rf build".into()],
            always: vec![],
            metadata: serde_json::json!({}),
        };
        // Danger pattern → the call blocks on an ask; prove an Asked event fires.
        let perm2 = std::sync::Arc::new(perm);
        let p = perm2.clone();
        let h = tokio::spawn(async move { p.ask("ses_1", req).await });
        let asked = rx
            .recv()
            .await
            .expect("danger op should emit an Asked event");
        perm2.reply(&asked.request_id, Reply::Once);
        h.await.unwrap().expect("approved once");
    }

    #[test]
    fn mode_defaults_and_set() {
        let perm = Permission::with_mode(Ruleset::new(), PermissionMode::AcceptEdits);
        assert_eq!(perm.mode("unknown_session"), PermissionMode::AcceptEdits);
        perm.set_mode("ses_1", PermissionMode::FullAuto);
        assert_eq!(perm.mode("ses_1"), PermissionMode::FullAuto);
        assert_eq!(perm.mode("ses_2"), PermissionMode::AcceptEdits); // isolated
    }

    #[test]
    fn child_inherits_parent_mode_via_chain() {
        let perm = Permission::new(Ruleset::new());
        perm.set_mode("ses_root", PermissionMode::FullAuto);
        // root → workflow → implementer: depth 2, mode set only on the root.
        perm.link_parent("ses_workflow", "ses_root");
        perm.link_parent("ses_impl", "ses_workflow");
        assert_eq!(perm.mode("ses_impl"), PermissionMode::FullAuto);
        assert_eq!(perm.mode("ses_workflow"), PermissionMode::FullAuto);
    }

    #[test]
    fn child_override_beats_parent() {
        let perm = Permission::new(Ruleset::new());
        perm.set_mode("ses_root", PermissionMode::FullAuto);
        perm.link_parent("ses_child", "ses_root");
        perm.set_mode("ses_child", PermissionMode::ApproveEach);
        assert_eq!(perm.mode("ses_child"), PermissionMode::ApproveEach);
        assert_eq!(perm.mode("ses_root"), PermissionMode::FullAuto);
    }

    #[tokio::test]
    async fn mid_run_parent_flip_changes_child_ask() {
        let perm = Permission::new(Ruleset::new());
        perm.link_parent("ses_child", "ses_root");

        // Parent flips to full-auto AFTER the link: the child's next ask must
        // auto-allow (live inheritance, not copy-at-spawn).
        perm.set_mode("ses_root", PermissionMode::FullAuto);
        let req = PermissionRequest {
            permission: "bash".into(),
            patterns: vec!["cargo test".into()],
            always: vec![],
            metadata: serde_json::json!({}),
        };
        perm.ask("ses_child", req)
            .await
            .expect("child ask auto-allows under parent's full-auto");
    }

    #[test]
    fn parent_cycle_does_not_hang() {
        let perm = Permission::with_mode(Ruleset::new(), PermissionMode::AcceptEdits);
        perm.link_parent("ses_a", "ses_b");
        perm.link_parent("ses_b", "ses_a");
        // Neither has a mode: the walk must terminate at the default.
        assert_eq!(perm.mode("ses_a"), PermissionMode::AcceptEdits);
    }
}
