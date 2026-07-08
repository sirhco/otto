//! Integration tests for auto-compaction: [`compaction::create`], the
//! [`run_loop`] auto-compaction pre-check, the [`ProcessOutcome::Compact`]
//! path, and [`compaction::prune`].
//!
//! A [`ScriptedRoute`] returns canned [`LLMEvent`] streams so the whole flow
//! runs headless — the same pattern as `run_loop.rs`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use otto_events::{FinishReason, LLMEvent, Usage};
use otto_llm::model::ModelLimits;
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_session::compaction;
use otto_session::{RunConfig, run_loop};
use otto_storage::model::{
    Assistant, AssistantPath, AssistantTime, CompletedTime, Info, InfoBody, Part, PartKind,
    TokenCache, Tokens, ToolState, User, UserModel, UserTime, new_message_id, new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{AllowAll, ToolRegistry};
use serde_json::json;
use tokio_util::sync::CancellationToken;

// -- scripted route ----------------------------------------------------------

struct ScriptedRoute {
    turns: Mutex<VecDeque<Vec<LLMEvent>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedRoute {
    fn build(turns: Vec<Vec<LLMEvent>>) -> (Arc<dyn Route>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let route = Arc::new(ScriptedRoute {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: calls.clone(),
        });
        (route, calls)
    }
}

impl Route for ScriptedRoute {
    fn id(&self) -> &str {
        "scripted"
    }
    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

// -- event helpers -----------------------------------------------------------

fn step_start() -> LLMEvent {
    LLMEvent::StepStart { index: 0 }
}
fn step_finish(reason: FinishReason, usage: Option<Usage>) -> LLMEvent {
    LLMEvent::StepFinish {
        index: 0,
        reason,
        usage,
        provider_metadata: None,
    }
}
fn finish(reason: FinishReason) -> LLMEvent {
    LLMEvent::Finish {
        reason,
        usage: None,
        provider_metadata: None,
    }
}
fn text_events(id: &str, text: &str) -> Vec<LLMEvent> {
    vec![
        LLMEvent::TextStart {
            id: id.into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: id.into(),
            text: text.into(),
            provider_metadata: None,
        },
        LLMEvent::TextEnd {
            id: id.into(),
            provider_metadata: None,
        },
    ]
}
/// A finished text-only turn: text then finish(stop).
fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    let mut t = vec![step_start()];
    t.extend(text_events(id, text));
    t.push(step_finish(FinishReason::Stop, None));
    t.push(finish(FinishReason::Stop));
    t
}

// -- fixtures ----------------------------------------------------------------

async fn open_session(id: &str) -> Store {
    let store = Store::open_in_memory().await.expect("store");
    store
        .create_session(&Session {
            id: id.into(),
            project_id: "prj_1".into(),
            parent_id: None,
            directory: "/work".into(),
            title: "Test".into(),
            version: "1.0.0".into(),
            cost: 0.0,
            tokens: SessionTokens::default(),
            metadata: None,
            time_created: 1,
            time_updated: 1,
        })
        .await
        .expect("session");
    store
}

async fn insert_user(store: &Store, ses: &str, text: &str) -> String {
    let id = new_message_id();
    store
        .insert_message(&Info {
            id: id.clone(),
            session_id: ses.into(),
            body: InfoBody::User(User {
                time: UserTime { created: 1 },
                format: None,
                summary: None,
                agent: "build".into(),
                model: UserModel {
                    provider_id: "anthropic".into(),
                    model_id: "claude-3".into(),
                    variant: None,
                },
                system: None,
                tools: None,
            }),
        })
        .await
        .expect("user");
    store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: ses.into(),
            message_id: id.clone(),
            kind: PartKind::Text {
                text: text.into(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            },
        })
        .await
        .expect("user text");
    id
}

async fn insert_assistant(
    store: &Store,
    ses: &str,
    parent: &str,
    text: &str,
    tokens: Tokens,
) -> String {
    let id = new_message_id();
    store
        .insert_message(&Info {
            id: id.clone(),
            session_id: ses.into(),
            body: InfoBody::Assistant(Assistant {
                time: AssistantTime {
                    created: 1,
                    completed: Some(1),
                },
                error: None,
                parent_id: parent.into(),
                model_id: "claude-3".into(),
                provider_id: "anthropic".into(),
                mode: "build".into(),
                agent: "build".into(),
                path: AssistantPath {
                    cwd: "/w".into(),
                    root: "/w".into(),
                },
                summary: None,
                cost: 0.0,
                tokens,
                structured: None,
                variant: None,
                finish: Some("stop".into()),
            }),
        })
        .await
        .expect("assistant");
    store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: ses.into(),
            message_id: id.clone(),
            kind: PartKind::Text {
                text: text.into(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            },
        })
        .await
        .expect("assistant text");
    id
}

async fn insert_tool_part(store: &Store, ses: &str, msg_id: &str, tool: &str, output: &str) {
    store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: ses.into(),
            message_id: msg_id.into(),
            kind: PartKind::Tool {
                call_id: new_part_id(),
                tool: tool.into(),
                metadata: None,
                state: ToolState::Completed {
                    input: json!({}),
                    output: output.into(),
                    title: tool.into(),
                    metadata: json!({}),
                    time: CompletedTime {
                        start: 1,
                        end: 2,
                        compacted: None,
                    },
                    attachments: None,
                },
            },
        })
        .await
        .expect("tool part");
}

fn zero_tokens() -> Tokens {
    Tokens {
        total: None,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: TokenCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

fn model_with_context(context: Option<u64>) -> Model {
    let mut m = Model::new("anthropic", "claude-3", "route_scripted");
    m.limits = ModelLimits {
        context,
        input: None,
        output: None,
    };
    m
}

fn config(store: Store, route: Arc<dyn Route>, model: Model) -> RunConfig {
    RunConfig {
        store,
        route,
        tools: Arc::new(ToolRegistry::new()),
        permission: Arc::new(AllowAll),
        model,
        agent: "build".into(),
        agent_prompt: Some("SYSTEM".into()),
        directory: std::env::temp_dir(),
        max_steps: None,
        abort: CancellationToken::new(),
        subagent: None,
        preserve_recent_tokens: 20_000,
        compaction_reserved: 0,
        auto_compact: true,
        max_retries: 5,
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    }
}

async fn parts_of(store: &Store, msg_id: &str) -> Vec<Part> {
    store.list_parts(msg_id).await.expect("parts")
}

/// The tool part matching `tool` on `msg_id`, if any.
async fn tool_state(store: &Store, msg_id: &str, tool: &str) -> Option<ToolState> {
    for p in parts_of(store, msg_id).await {
        if let PartKind::Tool { tool: t, state, .. } = &p.kind
            && t == tool
        {
            return Some(state.clone());
        }
    }
    None
}

// -- 1. compaction::create ---------------------------------------------------

#[tokio::test]
async fn create_summarizes_and_filter_reorders() {
    let ses = "ses_create";
    let store = open_session(ses).await;

    // Three turns; the first two are huge so `select` keeps only the last.
    let big = "x".repeat(4_000);
    let u1 = insert_user(&store, ses, &big).await;
    let a1 = insert_assistant(&store, ses, &u1, &big, zero_tokens()).await;
    let u2 = insert_user(&store, ses, &big).await;
    let a2 = insert_assistant(&store, ses, &u2, &big, zero_tokens()).await;
    let u3 = insert_user(&store, ses, "hi").await;
    let a3 = insert_assistant(&store, ses, &u3, "ok", zero_tokens()).await;

    // The compaction agent returns a canned summary.
    let (route, calls) = ScriptedRoute::build(vec![text_turn("s1", "SUMMARY")]);
    let mut cfg = config(store.clone(), route, model_with_context(None));
    cfg.preserve_recent_tokens = 60; // only the tiny last turn fits

    compaction::create(&cfg, ses, true, false)
        .await
        .expect("create");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one summary LLM call");

    // A user message carries a compaction part with tail_start_id = u3.
    let msgs = store.messages_with_parts(ses).await.expect("history");
    let compaction_msg = msgs
        .iter()
        .find(|m| m.parts.iter().any(Part::is_compaction))
        .expect("a compaction message");
    let tail = compaction_msg
        .parts
        .iter()
        .find_map(Part::compaction_tail_start_id)
        .expect("tail_start_id set");
    assert_eq!(tail, &u3, "tail begins at the last-turn user");

    // A summary assistant carries the canned summary text.
    let summary = msgs
        .iter()
        .find(|m| m.info.as_assistant().and_then(|a| a.summary) == Some(true))
        .expect("a summary assistant");
    assert_eq!(
        summary.info.as_assistant().unwrap().parent_id,
        compaction_msg.info.id
    );
    let summary_text = summary
        .parts
        .iter()
        .find_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("summary text part");
    assert_eq!(summary_text, "SUMMARY");

    // filter_compacted (fed newest-first) drops the summarized head and splices
    // [compaction, summary, tail…, continue].
    let mut newest_first = store.messages_with_parts(ses).await.expect("history");
    newest_first.reverse();
    let out = otto_storage::filter_compacted(newest_first);
    let out_ids: Vec<String> = out.iter().map(|m| m.info.id.clone()).collect();

    assert!(!out_ids.contains(&u1), "head dropped");
    assert!(!out_ids.contains(&a1), "head dropped");
    assert!(!out_ids.contains(&u2), "head dropped");
    assert!(!out_ids.contains(&a2), "head dropped");
    assert_eq!(out_ids[0], compaction_msg.info.id, "compaction first");
    assert_eq!(out_ids[1], summary.info.id, "summary second");
    assert_eq!(out_ids[2], u3, "retained tail start");
    assert_eq!(out_ids[3], a3, "retained tail");
    assert_eq!(out.len(), 5, "compaction, summary, tail(2), continue");
    // The last message is the synthetic auto-continue user.
    assert!(out.last().unwrap().info.is_user());
}

// -- 2. run_loop auto-compaction pre-check -----------------------------------

#[tokio::test]
async fn run_loop_auto_compacts_on_overflow() {
    let ses = "ses_auto";
    let store = open_session(ses).await;

    // A finished assistant whose tokens overflow the 1000-token context, plus a
    // pending user turn so the loop does not exit before the pre-check.
    let u1 = insert_user(&store, ses, "start").await;
    let overflow_tokens = Tokens {
        input: 5_000.0,
        ..zero_tokens()
    };
    insert_assistant(&store, ses, &u1, "prior", overflow_tokens).await;
    insert_user(&store, ses, "please continue").await;

    // Turn 0: the compaction summary. Turn 1: the post-compaction reply.
    let (route, calls) = ScriptedRoute::build(vec![
        text_turn("sum", "SUMMARY"),
        text_turn("rep", "resumed"),
    ]);
    let cfg = config(store.clone(), route, model_with_context(Some(1000)));

    run_loop(&cfg, ses).await.expect("run_loop");

    // Summary + reply were the two provider calls.
    assert_eq!(calls.load(Ordering::SeqCst), 2, "summary then reply");

    let msgs = store.messages_with_parts(ses).await.expect("history");
    assert!(
        msgs.iter().any(|m| m.parts.iter().any(Part::is_compaction)),
        "a compaction message was created before the turn"
    );
    assert!(
        msgs.iter()
            .any(|m| m.info.as_assistant().and_then(|a| a.summary) == Some(true)),
        "a summary assistant was created"
    );
}

// -- 3. ProcessOutcome::Compact path -----------------------------------------

#[tokio::test]
async fn run_loop_compact_outcome_continues() {
    let ses = "ses_compact";
    let store = open_session(ses).await;
    insert_user(&store, ses, "do a big thing").await;

    // Turn 0: a step whose usage overflows the 1000-token context → the
    // processor returns Compact. Turn 1: the compaction summary. Turn 2: reply.
    let overflow_turn = vec![
        step_start(),
        step_finish(
            FinishReason::Stop,
            Some(Usage {
                input_tokens: Some(5_000),
                ..Usage::default()
            }),
        ),
        finish(FinishReason::Stop),
    ];
    let (route, calls) = ScriptedRoute::build(vec![
        overflow_turn,
        text_turn("sum", "SUMMARY"),
        text_turn("rep", "resumed"),
    ]);
    let cfg = config(store.clone(), route, model_with_context(Some(1000)));

    let last = run_loop(&cfg, ses).await.expect("run_loop");

    // Compact → summary → reply: three provider calls, and the loop continued
    // (did not break) to produce the final reply.
    assert_eq!(calls.load(Ordering::SeqCst), 3, "compact, summary, reply");

    let msgs = store.messages_with_parts(ses).await.expect("history");
    assert!(
        msgs.iter().any(|m| m.parts.iter().any(Part::is_compaction)),
        "compaction ran on the Compact outcome"
    );
    let final_text = parts_of(&store, last.id())
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        });
    assert_eq!(
        final_text.as_deref(),
        Some("resumed"),
        "loop continued to reply"
    );
}

// -- 4. prune ----------------------------------------------------------------

#[tokio::test]
async fn prune_erases_old_tool_outputs() {
    let ses = "ses_prune";
    let store = open_session(ses).await;
    // ~25k tokens each (100k chars / 4).
    let big = "y".repeat(100_000);

    // 5 turns; the last two are protected. Tool outputs live on the assistants.
    let u1 = insert_user(&store, ses, "t1").await;
    let a1 = insert_assistant(&store, ses, &u1, "", zero_tokens()).await;
    insert_tool_part(&store, ses, &a1, "grep", &big).await;

    let u2 = insert_user(&store, ses, "t2").await;
    let a2 = insert_assistant(&store, ses, &u2, "", zero_tokens()).await;
    insert_tool_part(&store, ses, &a2, "grep", &big).await; // prunable
    insert_tool_part(&store, ses, &a2, "skill", &big).await; // protected tool

    let u3 = insert_user(&store, ses, "t3").await;
    let a3 = insert_assistant(&store, ses, &u3, "", zero_tokens()).await;
    insert_tool_part(&store, ses, &a3, "grep", &big).await; // within PROTECT budget

    let u4 = insert_user(&store, ses, "t4").await;
    insert_assistant(&store, ses, &u4, "no tools", zero_tokens()).await;

    let u5 = insert_user(&store, ses, "t5").await;
    let a5 = insert_assistant(&store, ses, &u5, "", zero_tokens()).await;
    insert_tool_part(&store, ses, &a5, "grep", &big).await; // last-2-turns protected

    let (route, _) = ScriptedRoute::build(vec![]);
    let cfg = config(store.clone(), route, model_with_context(None));

    compaction::prune(&cfg, ses).await.expect("prune");

    let compacted = |state: Option<ToolState>| match state {
        Some(ToolState::Completed { time, .. }) => time.compacted.is_some(),
        other => panic!("expected completed tool, got {other:?}"),
    };

    assert!(
        compacted(tool_state(&store, &a1, "grep").await),
        "old grep pruned"
    );
    assert!(
        compacted(tool_state(&store, &a2, "grep").await),
        "old grep pruned"
    );
    assert!(
        !compacted(tool_state(&store, &a2, "skill").await),
        "skill output protected"
    );
    assert!(
        !compacted(tool_state(&store, &a5, "grep").await),
        "last-2-turns protected"
    );
    // a3's grep sits within the PROTECT budget and is left intact.
    assert!(
        !compacted(tool_state(&store, &a3, "grep").await),
        "protected by PRUNE_PROTECT budget"
    );
}
