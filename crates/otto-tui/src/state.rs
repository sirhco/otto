//! The TUI application state and its `update` reducer.

use crate::client::{AgentInfo, ModelChoice, SessionInfo};
use crate::input::Editor;
use crate::sse::{PermissionAsked, ServerEvent, WfPhase};
use crossterm::event::KeyEvent;
use otto_events::LLMEvent;
use std::collections::{BTreeMap, HashSet};

/// Live view of an in-flight workflow run, folded from the `workflow.*`
/// events (the same events that fold transcript lines). Powers the progress
/// panel + cancel key (Task 6). `tasks` maps `task_index → (status, notes)`;
/// `done` is `None` while running, `Some(ok)` once the `Done` event lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowView {
    pub(crate) kind: String,
    pub(crate) arg: String,
    pub(crate) session: String,
    pub(crate) tasks: BTreeMap<u32, (String, String)>,
    pub(crate) done: Option<bool>,
}

/// One rendered line-group in the transcript.
#[derive(Debug, Clone)]
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Reasoning(String),
    /// A turn-fatal error (provider/transport failure, lost connection). Kept
    /// in the scrollback so the reason survives — the header status line alone
    /// is easy to miss and gets overwritten by the next turn.
    Error(String),
    /// A workflow lifecycle line (launch / started / per-task progress / done),
    /// folded from `ServerEvent::Workflow` and `Msg::StartWorkflow`. Rendered as
    /// a dim system line so the user can watch the run without it competing with
    /// assistant output.
    Workflow(String),
    Tool {
        name: String,
        status: ToolStatus,
        title: String,
        /// Parsed tool-call arguments (for the expanded detail view).
        input: Option<serde_json::Value>,
        /// Result / diff / error text (for the expanded detail view).
        output: Option<String>,
        /// Whether the user has expanded this tool's detail.
        expanded: bool,
    },
}

/// Lifecycle of a tool call as seen from the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

/// Status of a single todo item, as reported by the `todowrite` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    fn from_status_str(s: &str) -> Self {
        match s {
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            "cancelled" => TodoStatus::Cancelled,
            _ => TodoStatus::Pending,
        }
    }
}

/// A single todo item from the most recent `todowrite` tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoItem {
    pub(crate) content: String,
    pub(crate) status: TodoStatus,
}

/// Parse `input.todos` into a list; non-array / missing / malformed → empty vec.
pub(crate) fn parse_todos(input: &serde_json::Value) -> Vec<TodoItem> {
    input
        .get("todos")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .map(|it| TodoItem {
                    content: it
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: TodoStatus::from_status_str(
                        it.get("status").and_then(|s| s.as_str()).unwrap_or(""),
                    ),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Client-side pagination state for an in-progress `Overlay::Question` — one
/// question in `questions` at a time, `current` tracks which, `answers`
/// accumulates confirmed selections, `cursor` is the in-progress selection
/// for `questions[current]`.
#[derive(Debug, Clone)]
pub struct QuestionSession {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<crate::sse::QuestionPromptView>,
    pub current: usize,
    pub answers: Vec<Vec<usize>>,
    pub cursor: Vec<usize>,
    /// Highlighted option index in the current question's option list, for
    /// arrow-key navigation.
    pub highlight: usize,
}

impl QuestionSession {
    #[must_use]
    pub fn new(asked: crate::sse::QuestionAsked) -> Self {
        Self {
            id: asked.id,
            session_id: asked.session_id,
            questions: asked.questions,
            current: 0,
            answers: Vec::new(),
            cursor: Vec::new(),
            highlight: 0,
        }
    }

    fn current_question(&self) -> &crate::sse::QuestionPromptView {
        &self.questions[self.current]
    }

    /// Whether the current question allows multiple selections.
    #[must_use]
    pub fn current_question_is_multiple(&self) -> bool {
        self.current_question().multiple
    }

    /// Move the highlight cursor by `delta` (wrapping), clamped to the
    /// current question's option count.
    pub fn move_highlight(&mut self, delta: i32) {
        let len = self.current_question().options.len();
        if len == 0 {
            return;
        }
        let next = (self.highlight as i32 + delta).rem_euclid(len as i32);
        self.highlight = next as usize;
    }

    /// Toggle `index` in the in-progress `cursor` for the current question.
    /// On a non-multiple question, toggling replaces the cursor with just
    /// `index` (radio-button semantics); on a multiple question it
    /// adds/removes `index` (checkbox semantics).
    pub fn toggle(&mut self, index: usize) {
        if self.current_question().multiple {
            if let Some(pos) = self.cursor.iter().position(|&i| i == index) {
                self.cursor.remove(pos);
            } else {
                self.cursor.push(index);
            }
        } else {
            self.cursor = vec![index];
        }
    }

    /// Confirm the current question's `cursor` selection (must be
    /// non-empty), append it to `answers`, and advance. Returns `true` if
    /// this was the last question (the caller should now build a
    /// `Msg::QuestionReply::Answered(answers)`); `false` if more questions
    /// remain (or the cursor was empty and nothing advanced).
    pub fn confirm_current(&mut self) -> bool {
        if self.cursor.is_empty() {
            return false;
        }
        self.answers.push(std::mem::take(&mut self.cursor));
        self.highlight = 0;
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            false
        } else {
            true
        }
    }
}

/// The single active modal overlay, if any.
#[derive(Debug, Clone)]
pub enum Overlay {
    None,
    Help,
    Permission(PermissionAsked),
    Question(QuestionSession),
    Sessions,
    /// The multi-agent dashboard: top-level sessions other than the
    /// attached one, each with a derived busy/idle/awaiting-ask status, a
    /// peek panel for the selected row, and inline reply for a pending
    /// ask. State lives in `App.dashboard` (own selection cursor, not the
    /// shared `App.selected` the plain pickers use).
    Dashboard,
    Models,
    Agents,
    Palette(PaletteState),
    Files(FilePickerState),
    Search(SearchState),
    /// Free-text input box that returns arbitrary typed text on Enter (used to
    /// collect a workflow argument — a plan-file path or a TDD feature). Unlike
    /// every other overlay, it filters no known list; `kind` tags which
    /// workflow the typed text feeds.
    TextInput(TextInputState),
    /// Inline `@`-file/folder completion, anchored in the chat editor. Unlike
    /// every other overlay the editor keeps focus (the cursor stays in the
    /// buffer); this state only tracks the dropdown. The completion *query* is
    /// never stored — it is derived on demand from the editor buffer between
    /// the `@` anchor and the cursor (see [`App::mention_query`]).
    Mention(MentionState),
    /// Read-only progress panel for the in-flight workflow run, rendering
    /// `App.workflow` (the task→state map). Opened by the toggle key
    /// (`ctrl+w`) only while `App.workflow.is_some()`, so the render can
    /// assume `Some`.
    WorkflowStatus,
}

/// One session row in the multi-agent dashboard (`Overlay::Dashboard`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardRow {
    pub(crate) session: SessionInfo,
    pub(crate) status: DashboardStatus,
    /// Whether this row is a `workflow_task` child spliced in immediately
    /// after its `workflow_root` parent (rendered nested one level in, see
    /// Task 8). `false` for every primary row.
    pub(crate) indent: bool,
}

/// A dashboard row's derived status. Sorted `Awaiting* -> Busy -> Idle`
/// (needs-your-input first) by [`build_dashboard_rows`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DashboardStatus {
    /// The full pending permission ask — already carries everything the
    /// peek panel needs to render inline reply options, so selecting this
    /// row needs no extra fetch.
    AwaitingPermission(crate::sse::PermissionAsked),
    /// The full pending question ask (see [`AwaitingPermission`](Self::AwaitingPermission)
    /// for the same no-extra-fetch rationale).
    AwaitingQuestion(crate::sse::QuestionAsked),
    Busy,
    Idle,
}

/// Peek/reply UI state for the currently-selected dashboard row.
/// `Permission`/`Question` don't duplicate the ask data (already sitting in
/// the selected row's [`DashboardStatus`]) — this only tracks interaction-
/// transient state (or `Loading`/`Message` for the async idle/busy-row peek).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum DashboardPeek {
    #[default]
    Loading,
    /// The peeked session's latest response text (idle/busy rows).
    Message(String),
    /// A pending permission ask — render `Overlay::Permission`-style y/a/n.
    Permission,
    /// A pending single-question, single-select ask — render inline reply
    /// options. `highlight` is reserved for a future arrow-key cursor;
    /// v1 answers by number key (see Task 6), so it is always `0` today.
    Question { highlight: usize },
    /// A multi-select or multi-question ask (out of scope for inline
    /// reply — see this plan's Global Constraints) — the peek panel shows
    /// "press Enter to open this session" instead.
    NeedsFullSession,
}

/// Which input mode the dashboard overlay is currently in (Task 6 routes
/// keys through this; this task only defines the shape). `NewSession`
/// carries its own in-progress typed buffer inline rather than a separate
/// field on `DashboardState` that would be meaningless in the other two
/// modes. `Filter`'s live-typed text lives directly in
/// `DashboardState.filter` instead (so filtering updates live as you type,
/// no separate buffer to keep in sync — `Msg::DashboardFilterChanged` sets
/// `filter` directly).
// `Filter`/`NewSession` are only constructed by this task's own tests today —
// Task 6's key handling gets the first real caller (`/` and `c` respectively),
// same pattern as `latest_message_text`'s now-removed `#[allow(dead_code)]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub(crate) enum DashboardMode {
    #[default]
    Browsing,
    Filter,
    NewSession(String),
}

/// State for the multi-agent dashboard overlay (`Overlay::Dashboard`):
/// top-level sessions other than the attached one, each with a derived
/// status, a selection cursor, and the selected row's peek/reply state.
/// Deliberately has its own `selected` cursor rather than reusing the
/// shared `App.selected` the plain Sessions/Models/Agents pickers use —
/// this overlay's key handling (Task 6) is fully custom, not routed through
/// `picker_move`/`picker_confirm`.
#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardState {
    pub(crate) rows: Vec<DashboardRow>,
    pub(crate) selected: usize,
    pub(crate) peek: DashboardPeek,
    /// Session ids pinned to the top of the primary-row list (see
    /// `apply_pin_and_filter`). Survives dashboard reopen (`open_dashboard`
    /// preserves it), unlike `filter`/`mode`.
    pub(crate) pinned: HashSet<String>,
    /// Current input mode (Task 6 routes keys based on this).
    pub(crate) mode: DashboardMode,
    /// Currently-applied filter text (case-insensitive substring match
    /// against row titles, see `apply_pin_and_filter`); empty = no filter.
    /// Reset to empty on every dashboard reopen.
    pub(crate) filter: String,
    /// Set when a push event arrives (`session.created`, or a workflow
    /// `Started` phase) while the dashboard overlay is open — signals that
    /// the row list is stale and a full re-fetch is warranted sooner than
    /// the normal ~2s poll cadence, mirroring how `DashboardPeek::Loading`
    /// is a pure state marker `lib.rs`'s `maybe_fetch_dashboard_peek`
    /// polls for and acts on. Task 7's poll-cadence tick handler (`lib.rs`)
    /// should check this flag each tick alongside `dashboard_poll_due` and,
    /// if set, fetch immediately; it self-clears when the resulting
    /// `DashboardLoaded` message is folded (see that arm in `App::update`).
    pub(crate) needs_refetch: bool,
}

impl DashboardState {
    /// Compute the peek state for the currently-selected row from its
    /// derived status. `Busy`/`Idle` rows need an async fetch the caller
    /// triggers separately (this only returns the synchronous `Loading`
    /// placeholder for them); ask rows resolve immediately since their
    /// full prompt already came down with the periodic poll.
    pub(crate) fn derive_peek(&self) -> DashboardPeek {
        match self.rows.get(self.selected).map(|r| &r.status) {
            Some(DashboardStatus::AwaitingPermission(_)) => DashboardPeek::Permission,
            Some(DashboardStatus::AwaitingQuestion(q))
                if q.questions.len() == 1 && !q.questions[0].multiple =>
            {
                DashboardPeek::Question { highlight: 0 }
            }
            Some(DashboardStatus::AwaitingQuestion(_)) => DashboardPeek::NeedsFullSession,
            Some(DashboardStatus::Busy | DashboardStatus::Idle) | None => DashboardPeek::Loading,
        }
    }
}

/// Derive a row's status from a poll's permission/question responses: an
/// outstanding ask always wins over the session's own `busy` flag.
fn derive_dashboard_status(
    s: &SessionInfo,
    permissions: &[crate::sse::PermissionAsked],
    questions: &[crate::sse::QuestionAsked],
) -> DashboardStatus {
    permissions
        .iter()
        .find(|p| p.session_id == s.id)
        .map(|p| DashboardStatus::AwaitingPermission(p.clone()))
        .or_else(|| {
            questions
                .iter()
                .find(|q| q.session_id == s.id)
                .map(|q| DashboardStatus::AwaitingQuestion(q.clone()))
        })
        .unwrap_or(if s.busy {
            DashboardStatus::Busy
        } else {
            DashboardStatus::Idle
        })
}

/// Build sorted dashboard rows from a poll's three responses.
///
/// Primary rows are top-level sessions (`parent_id.is_none()`) plus workflow
/// roots (`kind == "workflow_root"`, which do carry a `parent_id` — the
/// session that launched the workflow — but still surface as their own
/// dashboard row), excluding the currently-attached session. These are
/// sorted `AwaitingAsk -> Busy -> Idle`, tie-broken within a status by
/// `time_updated` descending (most-recently-active first, same recency bias
/// `most_recent_session` already uses for session reopening) — unchanged
/// from before this task.
///
/// A second pass then splices `workflow_task` children in immediately after
/// their `workflow_root` parent's row (`indent: true`), grouped by
/// `parent_id` and ordered oldest-`time_updated`-first within a group (task
/// order roughly matches creation recency). A `workflow_task` whose parent
/// isn't a primary row in this list (e.g. the parent itself got excluded)
/// is dropped rather than surfaced as an orphan. Ad-hoc `subagent`/kindless
/// sessions with a `parent_id` are excluded from both passes, unchanged.
///
/// Does not apply pin ordering or the title filter — see
/// `apply_pin_and_filter`, which the caller runs over this function's
/// result. Keeping the two separate means this function's own direct unit
/// tests continue to exercise grouping in isolation.
pub(crate) fn build_dashboard_rows(
    sessions: &[SessionInfo],
    permissions: &[crate::sse::PermissionAsked],
    questions: &[crate::sse::QuestionAsked],
    attached: Option<&str>,
) -> Vec<DashboardRow> {
    let mut rows: Vec<DashboardRow> = sessions
        .iter()
        .filter(|s| {
            (s.parent_id.is_none() || s.kind.as_deref() == Some("workflow_root"))
                && Some(s.id.as_str()) != attached
        })
        .map(|s| DashboardRow {
            status: derive_dashboard_status(s, permissions, questions),
            session: s.clone(),
            indent: false,
        })
        .collect();
    rows.sort_by_key(|r| {
        let rank = match r.status {
            DashboardStatus::AwaitingPermission(_) | DashboardStatus::AwaitingQuestion(_) => 0,
            DashboardStatus::Busy => 1,
            DashboardStatus::Idle => 2,
        };
        (rank, std::cmp::Reverse(r.session.time_updated))
    });

    let mut children_by_parent: std::collections::HashMap<&str, Vec<&SessionInfo>> =
        std::collections::HashMap::new();
    for s in sessions {
        if s.kind.as_deref() == Some("workflow_task")
            && let Some(parent) = s.parent_id.as_deref()
        {
            children_by_parent.entry(parent).or_default().push(s);
        }
    }
    for group in children_by_parent.values_mut() {
        group.sort_by_key(|s| s.time_updated);
    }

    let mut out = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let parent_id = row.session.id.clone();
        out.push(row);
        if let Some(children) = children_by_parent.get(parent_id.as_str()) {
            for child in children {
                out.push(DashboardRow {
                    status: derive_dashboard_status(child, permissions, questions),
                    session: (*child).clone(),
                    indent: true,
                });
            }
        }
    }
    out
}

/// Reorder `rows` so pinned primary rows float to the top and drop rows
/// that don't survive `filter` — the pin/filter pass `build_dashboard_rows`
/// deliberately leaves out (see its doc comment).
///
/// Pin-priority: primary rows (`!indent`) are stably partitioned into
/// pinned-first / unpinned, each half keeping its incoming relative order
/// (no re-sort by rank/recency) — each primary row's indented children (if
/// any) travel with it, unaffected by which half the parent lands in.
///
/// Filter (skipped entirely when `filter` is empty): a case-insensitive
/// substring match against `session.title` (`None` title treated as empty,
/// matching the title-rendering convention elsewhere in this file). A
/// primary row is kept if its own title matches OR any indented child's
/// title matches, so a parent with a matching child stays visible even
/// when the parent's own title doesn't — but a kept parent's children are
/// still individually filtered (a non-matching child of a kept parent is
/// hidden). A filtered-out primary row takes its children with it
/// regardless of their own titles.
pub(crate) fn apply_pin_and_filter(
    rows: Vec<DashboardRow>,
    pinned: &HashSet<String>,
    filter: &str,
) -> Vec<DashboardRow> {
    // Re-group into (parent, children) blocks so the pin partition and the
    // filter both move a parent and its children together.
    let mut groups: Vec<(DashboardRow, Vec<DashboardRow>)> = Vec::new();
    for row in rows {
        if row.indent {
            if let Some((_, children)) = groups.last_mut() {
                children.push(row);
            }
            // An indented row with no preceding parent group shouldn't
            // occur (`build_dashboard_rows` always emits parent-then-
            // children) — silently drop rather than panic if it ever did.
        } else {
            groups.push((row, Vec::new()));
        }
    }

    let (pinned_groups, unpinned_groups): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .partition(|(row, _)| pinned.contains(&row.session.id));

    let filter_lower = filter.to_lowercase();
    let title_matches = |row: &DashboardRow| -> bool {
        filter.is_empty()
            || row
                .session
                .title
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(filter_lower.as_str())
    };

    let mut out = Vec::new();
    for (parent, children) in pinned_groups.into_iter().chain(unpinned_groups) {
        if !filter.is_empty() && !title_matches(&parent) && !children.iter().any(title_matches) {
            continue;
        }
        out.push(parent);
        out.extend(children.into_iter().filter(title_matches));
    }
    out
}

/// Extract a lightweight preview of the last assistant text part across
/// `rows` (the raw JSON returned by `GET /session/{id}/message`) — used
/// for the dashboard's peek panel, which shows a short preview rather than
/// running the full markdown transcript pipeline `load_history` does for
/// the attached session. Matches the wire shape of `otto_storage::WithParts`
/// (`{"info": {"role": "user"|"assistant", ...}, "parts": [{"type": "text", "text": ...}, ...]}`).
pub(crate) fn latest_message_text(rows: &[serde_json::Value]) -> String {
    rows.iter()
        .rev()
        .find(|row| row["info"]["role"] == "assistant")
        .and_then(|row| {
            row["parts"]
                .as_array()?
                .iter()
                .rev()
                .find(|p| p["type"] == "text")?
                .get("text")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "(no response yet)".to_string())
}

/// State for the free-text input overlay (`Overlay::TextInput`): the prompt
/// `title`, the current typed `query`, and the workflow `kind` (`sdd`/`plan`/
/// `tdd`) the text will be dispatched to on Enter.
#[derive(Debug, Default, Clone)]
pub struct TextInputState {
    pub title: String,
    pub query: String,
    pub kind: String,
    /// Active inline `@`-mention completion within this text box, if any. The
    /// query is `&query[anchor + 1..]` (single-line, cursor always at the end).
    pub(crate) mention: Option<TextMentionState>,
}

/// Inline `@`-mention completion state for the chat editor
/// ([`Overlay::Mention`]). The `@` sits at byte offset `anchor_col` on logical
/// row `anchor_row`; the live query is derived from the editor buffer
/// ([`App::mention_query`]) rather than stored, so backspacing/typing never
/// desyncs it from what the user sees.
#[derive(Debug, Clone)]
pub struct MentionState {
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) selected: usize,
    pub(crate) results: Vec<String>,
    pub(crate) truncated: bool,
    pub(crate) loading: bool,
}

/// Inline `@`-mention completion state for a [`TextInputState`] box. `anchor`
/// is the byte offset of the `@` within the box's `query`; the live query is
/// `&query[anchor + 1..]`.
#[derive(Debug, Default, Clone)]
pub struct TextMentionState {
    pub(crate) anchor: usize,
    pub(crate) selected: usize,
    pub(crate) results: Vec<String>,
    pub(crate) truncated: bool,
    pub(crate) loading: bool,
}

/// State for the file-attachment picker overlay: the typed query, the
/// currently selected index into `file_matches(results, query)`, the raw
/// (unfiltered) result set from the last `/file/list` fetch, whether the
/// server truncated that list, and whether a fetch is in flight.
#[derive(Debug, Default, Clone)]
pub struct FilePickerState {
    pub(crate) query: String,
    pub(crate) selected: usize,
    pub(crate) results: Vec<String>,
    pub(crate) truncated: bool,
    pub(crate) loading: bool,
}

/// State for the ctrl+k command palette overlay: the typed query and the
/// currently selected index into `palette_matches(query)`.
#[derive(Debug, Default, Clone)]
pub struct PaletteState {
    pub(crate) query: String,
    pub(crate) selected: usize,
}

/// State for the `/` transcript-search overlay: the typed query and the
/// current-match ordinal. `current` is an index into whatever
/// `search_matches(...)` returns at render time (not a width-dependent line
/// number), so it survives frame-to-frame without any render context.
#[derive(Debug, Default, Clone)]
pub struct SearchState {
    pub(crate) query: String,
    pub(crate) current: usize,
}

/// The TUI-local mirror of the wire-level question reply (`Answered`/
/// `Cancelled`) — kept separate from `otto_tools::QuestionOutcome` so this
/// crate's `state.rs` doesn't need an `otto-tools` dependency just for this
/// one enum (it already has one transitively via other otto crates, but the
/// direct type stays TUI-local for the same reason `PermissionReply` carries
/// a plain `String` rather than `otto_permission::Reply`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionReplyKind {
    Answered(Vec<Vec<usize>>),
    Cancelled,
}

/// Everything the loop can hand to [`App::update`].
///
/// `Server(LLMEvent)` is the largest variant by a wide margin, but `Msg`
/// values are short-lived (constructed, matched once, dropped) rather than
/// stored in bulk, so the size spread is not worth boxing away.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Key(KeyEvent),
    Resize,
    SessionsLoaded(Vec<SessionInfo>),
    /// `GET /session` + `GET /permission` + `GET /question` all resolved —
    /// the dashboard's periodic poll result.
    DashboardLoaded {
        sessions: Vec<SessionInfo>,
        permissions: Vec<crate::sse::PermissionAsked>,
        questions: Vec<crate::sse::QuestionAsked>,
    },
    /// The peeked session's latest message fetched (idle/busy row peek).
    DashboardPeekLoaded {
        session_id: String,
        text: String,
    },
    AgentsLoaded(Vec<AgentInfo>),
    ModelsLoaded(Vec<ModelChoice>),
    HistoryLoaded(Vec<serde_json::Value>),
    Server(LLMEvent),
    Event(ServerEvent),
    Submitted(String),
    Error(String),
    Quit,
    PermissionReply {
        id: String,
        reply: String,
    },
    QuestionReply {
        id: String,
        reply: QuestionReplyKind,
    },
    SwitchSession(String),
    /// Create a fresh session and switch to it (clears the transcript). The
    /// loop performs the HTTP create, then routes a `SwitchSession`.
    NewSession,
    /// The prompt stream ended (terminal event or connection close). Clears a
    /// stuck "…thinking" status if no `finish`/error event already resolved it.
    PromptEnded,
    /// Toggle the expanded/collapsed detail view of the most-recent tool row.
    ToggleTool,
    /// Scroll the transcript toward older content (away from the bottom).
    ScrollUp,
    /// Scroll the transcript toward the newest content (toward the bottom).
    ScrollDown,
    /// Jump back to following the newest content.
    ScrollBottom,
    /// Periodic tick (8/s) driving the spinner + elapsed-seconds liveness
    /// indicator while a prompt streams.
    Tick,
    /// The `/file/list` fetch for the file-attachment picker completed.
    FilesLoaded(Vec<String>, bool),
    /// Toggle the collapsed/expanded state of the todo panel.
    ToggleTodos,
    /// Launch a workflow (`kind` = `sdd`/`plan`/`tdd`, `arg` = plan-file path or
    /// feature text). `dispatch` (lib.rs) turns this into a detached
    /// `client.workflow(kind, arg)` call; progress arrives on the `/event` pump.
    StartWorkflow {
        kind: String,
        arg: String,
    },
    /// Cancel the in-flight workflow bound to `session`. `dispatch` (lib.rs)
    /// turns this into a detached `client.cancel_workflow(session)` call; the
    /// server-side cancel surfaces as a `workflow.done{ok:false}` event.
    CancelWorkflow(String),
    /// Interrupt the in-flight prompt turn for `session` without ending the
    /// session (Esc while busy). `dispatch` (lib.rs) turns this into a detached
    /// `client.cancel_run(session)` call; the run aborts and the stream settles.
    InterruptTurn(String),
    /// Advance the per-session permission mode to the next in the cycle
    /// (`approve-each` → `accept-edits` → `full-auto` → `approve-each`).
    /// `dispatch` (lib.rs) sets `App.permission_mode` optimistically and, if a
    /// session is active, fires the `client.set_permission_mode` call; the
    /// server confirms (or corrects) via a `permission.mode_changed` event.
    CyclePermissionMode,
    /// The server confirmed the session's permission mode (via
    /// `permission.mode_changed` on the `/event` stream, translated in the
    /// event pump). Syncs `App.permission_mode` to the authoritative value.
    PermissionModeChanged(String),
    /// Terminal focus changed (crossterm focus-change events, enabled at
    /// startup). Gates the turn-finished OS notification.
    FocusChanged(bool),
    /// The periodic OS-appearance poll ran again. `None` = undetectable this
    /// round (unsupported platform/desktop environment, or a transient
    /// command failure) — leaves the active theme untouched.
    OsThemeChanged(Option<crate::appearance::ThemeMode>),
    /// The dashboard's "new session" inline input (`DashboardMode::NewSession`)
    /// was submitted with this typed title. `dispatch` (lib.rs, Task 7) turns
    /// this into a `client.create_session(&title)` call and, on success,
    /// routes a `DashboardSessionCreated`.
    CreateDashboardSession(String),
    /// A `CreateDashboardSession` call succeeded — insert the new session as
    /// a fresh primary row and select it.
    DashboardSessionCreated(SessionInfo),
    /// Toggle the currently-selected dashboard row's session id in/out of
    /// `App.dashboard.pinned`, then re-apply pin/filter ordering immediately.
    DashboardTogglePin,
    /// The dashboard filter's typed text changed (live, as-you-type — Task 6
    /// dispatches this on every keystroke in `DashboardMode::Filter`). Sets
    /// `App.dashboard.filter` and re-applies pin/filter ordering immediately.
    DashboardFilterChanged(String),
}

/// A side effect the event loop must perform because it needs the terminal or
/// stdout writer, which `App::update` has no access to. Drained by `event_loop`
/// after each message (mirrors the `should_quit` polling pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopAction {
    /// Copy the last assistant message to the system clipboard (OSC-52).
    Yank,
    /// Suspend to the shell (SIGTSTP) and re-enter on resume.
    Suspend,
    /// Write the turn-finished OS notification (OSC 9) and set a "done"
    /// terminal title.
    Notify,
    /// Reset the terminal title back to the plain `otto` title.
    ResetTitle,
    /// Re-apply the OSC 12 cursor color for the newly active theme (after an
    /// OS-appearance swap).
    CursorColor,
}

/// The whole TUI state.
#[derive(Debug)]
pub struct App {
    pub transcript: Vec<TranscriptItem>,
    pub overlay: Overlay,
    pub sessions: Vec<SessionInfo>,
    /// State for `Overlay::Dashboard` (Task 5).
    pub(crate) dashboard: DashboardState,
    pub session_id: Option<String>,
    pub agents: Vec<AgentInfo>,
    pub models: Vec<ModelChoice>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub status: String,
    pub should_quit: bool,
    /// Lines-from-bottom scroll position in WRAPPED rows (0 = follow). u32:
    /// long sessions exceed u16 rows; the render slices the line list so the
    /// widget-level u16 scroll offset never overflows.
    pub scroll: u32,
    /// Selected tool row for per-row expand, as an index into `transcript`.
    /// `None` = no selection (normal follow/scroll). See `select_*_tool`.
    pub(crate) tool_cursor: Option<usize>,
    pub input: Editor,
    /// A terminal/stdout side effect for the event loop to perform, if any.
    /// Set by `on_key`, drained by `event_loop` (like `should_quit`).
    pub pending_action: Option<LoopAction>,
    /// Whether otto's terminal currently has focus (crossterm focus-change
    /// events). Gates the turn-finished OS notification — no point notifying
    /// a user who's already looking at the screen.
    pub focused: bool,
    /// Whether a "done" terminal title is currently set, awaiting reset on
    /// the next focus-gained event.
    pub(crate) title_active: bool,
    /// Index of the open assistant text item currently being streamed, if any.
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    /// Highlighted row within the active list overlay (Sessions/Models/Agents).
    pub selected: usize,
    /// Current spinner animation frame, advanced once per `Msg::Tick` while busy.
    pub spinner_frame: usize,
    /// Ticks elapsed since the current busy status began (reset when idle).
    pub busy_ticks: u32,
    /// Monotonic tick counter, advanced every `Msg::Tick` regardless of busy
    /// state (unlike `busy_ticks`, which resets when idle). Drives flash expiry.
    pub tick: u32,
    /// Transient auto-fading status confirmation, if any.
    pub(crate) flash: Option<Flash>,
    /// Last-seen token usage from a `StepFinish`/`Finish` event, if any.
    pub usage: Option<otto_events::Usage>,
    /// Paths attached to the next prompt submission via the ctrl+f file picker.
    pub(crate) attachments: Vec<String>,
    /// Paths accepted via inline `@`-mentions, kept separate from
    /// [`attachments`](Self::attachments) so the ctrl+f flow is untouched. At
    /// submit time these are filtered to the ones whose `@path` token still
    /// survives in the message text (see [`take_files_for_submit`]).
    pub(crate) mention_paths: Vec<String>,
    /// Latest todo list from a `todowrite` tool call, if any.
    pub(crate) todos: Vec<TodoItem>,
    /// Whether the todo panel is collapsed.
    pub(crate) todos_collapsed: bool,
    /// Active style tokens (dark by default; config-driven via `Theme::select_with` at startup).
    pub theme: crate::theme::Theme,
    /// Terminal color depth, detected once at startup from `COLORTERM`/
    /// `TERM`. Used to re-quantize `dark_theme`/`light_theme` on an
    /// OS-appearance swap; unused outside `theme = "auto"` mode.
    pub(crate) color_depth: crate::appearance::ColorDepth,
    /// Precomputed, already-quantized dark preset for `theme = "auto"` mode.
    /// Only meaningful when `theme_mode.is_some()`.
    pub(crate) dark_theme: crate::theme::Theme,
    /// Precomputed, already-quantized light preset for `theme = "auto"`
    /// mode. Only meaningful when `theme_mode.is_some()`.
    pub(crate) light_theme: crate::theme::Theme,
    /// The currently-active OS appearance, if auto-detection is in effect.
    /// `None` means `theme != "auto"` (or detection hasn't resolved yet) —
    /// used to dedupe repeated `Msg::OsThemeChanged` polls reporting no
    /// change.
    pub(crate) theme_mode: Option<crate::appearance::ThemeMode>,
    /// Bumped whenever the assembled transcript lines would change, so the
    /// render-side `LineCache` (view.rs) knows to reassemble. View-only
    /// messages (scroll, tick, overlay open/close, search nav) do NOT bump it.
    pub render_gen: u64,
    /// Memoized transcript render, keyed by `render_gen` + render width.
    /// `RefCell` because `view::transcript` only holds `&App`.
    pub line_cache: std::cell::RefCell<Option<LineCache>>,
    /// The scroll bound (`wrap_total - viewport height`) published by the last
    /// transcript render, so `scroll_up` can clamp instead of building
    /// invisible overscroll debt. `Cell` because `view::transcript` only holds
    /// `&App` (same pattern as `line_cache`).
    pub last_scroll_max: std::cell::Cell<u32>,
    /// Startup splash: ticks remaining before it auto-dismisses (`Some(n)` while
    /// showing, `None` once dismissed). Set at launch by `run`, decremented each
    /// `Msg::Tick`, and cleared immediately by any keypress (`on_key`).
    pub splash: Option<u16>,
    /// Live view of the in-flight workflow run, folded from `workflow.*`
    /// events. `None` until a run starts; a fresh `Started` resets it.
    pub(crate) workflow: Option<WorkflowView>,
    /// Current per-session permission mode (`approve-each`/`accept-edits`/
    /// `full-auto`), shown in the header and cycled by shift+tab. Kept as a
    /// plain wire-format string rather than `otto_permission::PermissionMode`
    /// to avoid adding that dependency to this dependency-light crate.
    pub permission_mode: String,
    /// Live retry-backoff countdown, set by [`LLMEvent::Retry`] and re-rendered
    /// into `status` on every tick so the header counts down instead of
    /// freezing on the snapshot taken when the event arrived. Cleared by the
    /// next non-`Retry` stream event (the retried attempt is live again).
    pub(crate) retry: Option<RetryCountdown>,
    /// Real session-total tokens `(input, output)`, accumulated once per
    /// assistant message on its terminal `Finish` (see `fold_event`). Rendered
    /// as the `Σ` suffix of [`usage_line`](Self::usage_line) — the honest
    /// number for comparing runs (e.g. tersemode on vs off): measured usage,
    /// no estimation. Reset on session switch.
    pub session_tokens: (u64, u64),
    /// Last usage reading for the in-flight assistant message (StepFinish and
    /// Finish REPLACE it — mirroring the processor's `a.tokens = tokens` —
    /// so duplicated readings can't double-count). Drained into
    /// `session_tokens` by the message's terminal `Finish`.
    pub(crate) msg_usage: Option<(u64, u64)>,
    /// Live tool-call ids mapped to their transcript row, so results landing
    /// out of submission order (parallel tools) attach to the right row.
    /// Entries are removed as tools finish; cleared on submit/history reload.
    pub(crate) running_tools: Vec<(String, usize)>,
    /// Transcript index where the in-flight assistant message's items begin.
    /// A mid-stream retry purges that message's parts server-side and
    /// re-streams them, so the fold rolls the transcript back to this point on
    /// [`LLMEvent::Retry`] instead of leaving the partial attempt as an
    /// orphaned duplicate. `None` when no message is streaming.
    pub(crate) msg_start: Option<usize>,
}

/// `(input, output)` token counts from a [`otto_events::Usage`], zeroing
/// absent fields — the shape accumulated into `App::session_tokens`.
fn usage_in_out(u: &otto_events::Usage) -> (u64, u64) {
    (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0))
}

/// Human token count: `812` below 1k, `12.3k` above.
fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// State backing the live retry-backoff countdown in the header.
#[derive(Debug, Clone)]
pub(crate) struct RetryCountdown {
    /// Status prefix, e.g. `"rate-limited — retrying 2/5"`.
    pub prefix: String,
    /// Tick at which the backoff wait elapses.
    pub expires_tick: u32,
}

/// Memoized transcript render (assembled in view.rs). Line assembly (markdown
/// parse + tool render) is expensive and width-independent, so cache it keyed by
/// `App.render_gen` and the render width; rebuild only when either changes.
/// Highlighting is applied to a per-frame copy so the cached base never
/// accumulates match spans.
#[derive(Debug)]
pub struct LineCache {
    pub(crate) r#gen: u64,
    pub(crate) width: u16,
    pub(crate) lines: Vec<ratatui::text::Line<'static>>,
    /// Total WRAPPED rows at `width` (u32: long sessions exceed u16 rows).
    pub(crate) wrap_total: u32,
    /// Assembled-line index at which each `transcript[i]` begins. Content-derived
    /// (same key as `lines`), used to highlight + center the selected tool row.
    pub(crate) item_line_starts: Vec<usize>,
    /// Wrapped-row index at which each assembled line begins (prefix sums of
    /// per-line wrap counts; `line_wrap_starts.len() == lines.len()`). Maps
    /// logical line indices (search matches, item starts) to viewport rows,
    /// and lets the render slice the line list so `Paragraph::scroll`'s u16
    /// offset never overflows.
    pub(crate) line_wrap_starts: Vec<u32>,
    /// Per-item render memo: a rebuild re-renders only items whose fingerprint
    /// changed (during streaming that's just the open block), so cost per delta
    /// is O(open item), not O(whole transcript).
    pub(crate) items: Vec<ItemCacheEntry>,
}

/// One transcript item's memoized render (see [`LineCache::items`]). `lines`
/// is an `Arc` so reuse across rebuilds is a pointer copy — and testable via
/// `Arc::ptr_eq`.
#[derive(Debug, Clone)]
pub(crate) struct ItemCacheEntry {
    /// Content hash of the item (see `view::item_fingerprint`).
    pub(crate) fingerprint: u64,
    /// The item's assembled lines.
    pub(crate) lines: std::sync::Arc<Vec<ratatui::text::Line<'static>>>,
    /// Per assembled line, its wrapped row count at the cache's width. The
    /// item's total is their sum (accumulated into `LineCache::wrap_total`).
    pub(crate) line_wraps: std::sync::Arc<Vec<u16>>,
}

/// Whether a [`Flash`] is a positive confirmation or a warning — controls
/// the glyph/color the header renders it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlashKind {
    Success,
    Warning,
}

/// A transient, auto-fading status confirmation ("copied", "attached") or
/// warning ("turn in flight"). Tick-count based (no wall clock) so it stays
/// deterministic and testable.
#[derive(Debug, Clone)]
pub(crate) struct Flash {
    pub(crate) msg: String,
    pub(crate) kind: FlashKind,
    pub(crate) expires_tick: u32,
}

/// Ticks a flash stays visible before auto-fading. 16 ticks ≈ 2s at 8/s.
pub(crate) const FLASH_TICKS: u32 = 16;

/// Smallest cost (in dollars) that rounds up to a nonzero `{:.3}` display
/// (i.e. `$0.001`). Costs below this render identically to a genuine `$0`
/// result, so [`App::usage_line`] omits the segment entirely rather than
/// showing the misleading `$0.000`.
const MIN_DISPLAYED_COST: f64 = 0.0005;

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            transcript: Vec::new(),
            overlay: Overlay::None,
            sessions: Vec::new(),
            dashboard: DashboardState::default(),
            session_id: None,
            agents: Vec::new(),
            models: Vec::new(),
            agent: None,
            model: None,
            status: "connecting…".into(),
            should_quit: false,
            scroll: 0,
            tool_cursor: None,
            input: Editor::new(),
            pending_action: None,
            focused: true,
            title_active: false,
            open_text: None,
            open_reasoning: None,
            selected: 0,
            spinner_frame: 0,
            busy_ticks: 0,
            tick: 0,
            flash: None,
            usage: None,
            attachments: Vec::new(),
            mention_paths: Vec::new(),
            todos: Vec::new(),
            todos_collapsed: false,
            theme: crate::theme::Theme::dark(),
            color_depth: crate::appearance::ColorDepth::TrueColor,
            dark_theme: crate::theme::Theme::dark(),
            light_theme: crate::theme::Theme::dark(),
            theme_mode: None,
            render_gen: 0,
            line_cache: std::cell::RefCell::new(None),
            last_scroll_max: std::cell::Cell::new(0),
            splash: None,
            workflow: None,
            permission_mode: "approve-each".to_string(),
            retry: None,
            session_tokens: (0, 0),
            msg_usage: None,
            running_tools: Vec::new(),
            msg_start: None,
        }
    }

    /// Bumped whenever the assembled transcript lines would change (content
    /// mutation), so the render-side `LineCache` knows to reassemble.
    fn bump_render(&mut self) {
        self.render_gen = self.render_gen.wrapping_add(1);
    }

    /// Record where the in-flight assistant message's transcript items begin
    /// (first content event wins), so a mid-stream retry can roll them back.
    fn mark_msg_start(&mut self) {
        if self.msg_start.is_none() {
            self.msg_start = Some(self.transcript.len());
        }
    }

    /// Test-only hook for `view.rs` cache tests, which construct transcript
    /// state directly (bypassing `update`) and need to invalidate the cache
    /// without reaching for a private method.
    #[cfg(test)]
    pub fn bump_render_for_test(&mut self) {
        self.bump_render();
    }

    /// Whether the app is currently waiting on the server (spinner + elapsed
    /// indicator should animate in the header).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.status.contains("thinking")
            || self.status.contains("loading")
            || self.status.contains("new session")
            || self.status.contains("connecting")
            || self.status.contains("retrying")
    }

    /// Whether a prompt turn is actively generating and thus interruptible —
    /// the narrow subset of [`is_busy`](Self::is_busy) for which the server has
    /// a live run token registered (a submitted turn, or a mid-turn retry).
    /// Excludes transient startup states (connecting/loading/new session) that
    /// have no cancellable run behind them.
    #[must_use]
    pub fn turn_in_flight(&self) -> bool {
        self.status.contains("thinking") || self.status.contains("retrying")
    }

    /// `(name, title)` of the newest tool still `Running`, if any. Backs the
    /// activity mode-line above the input.
    #[must_use]
    pub(crate) fn running_tool(&self) -> Option<(&str, &str)> {
        self.transcript.iter().rev().find_map(|it| match it {
            TranscriptItem::Tool {
                name,
                status: ToolStatus::Running,
                title,
                ..
            } => Some((name.as_str(), title.as_str())),
            _ => None,
        })
    }

    /// Show a transient confirmation in the status slot for `FLASH_TICKS`.
    pub(crate) fn flash(&mut self, msg: impl Into<String>) {
        self.flash_with_kind(msg, FlashKind::Success);
    }

    /// Same as [`Self::flash`] but rendered with a warning glyph/color
    /// instead of the success checkmark.
    pub(crate) fn flash_warning(&mut self, msg: impl Into<String>) {
        self.flash_with_kind(msg, FlashKind::Warning);
    }

    fn flash_with_kind(&mut self, msg: impl Into<String>, kind: FlashKind) {
        self.flash = Some(Flash {
            msg: msg.into(),
            kind,
            expires_tick: self.tick.wrapping_add(FLASH_TICKS),
        });
    }

    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    pub fn open_picker(&mut self, overlay: Overlay) {
        self.selected = 0;
        self.overlay = overlay;
    }

    /// Open the dashboard, clearing any stale rows/peek/filter/mode from a
    /// previous time it was open (the caller — `route_message`, Task 8 —
    /// spawns the fresh fetch right after this returns). `pinned` survives
    /// the reset — it's a deliberate favorites list worth persisting across
    /// reopens, unlike a stale invisible filter, which would be a footgun.
    pub fn open_dashboard(&mut self) {
        let pinned = std::mem::take(&mut self.dashboard.pinned);
        self.dashboard = DashboardState {
            pinned,
            ..DashboardState::default()
        };
        self.overlay = Overlay::Dashboard;
    }

    /// Flip a dashboard row's status in place from a push event
    /// (`session.busy`/`session.idle`), without waiting for the next poll.
    /// No-op if `session_id` isn't currently a dashboard row (dashboard
    /// closed, row filtered out, or a `workflow_task` child — pushes only
    /// name a `session_id`, not a row identity, so a stale/unrelated id is
    /// simply not found). Never clobbers a pending ask
    /// (`AwaitingPermission`/`AwaitingQuestion`) — an ask the poll already
    /// surfaced always takes precedence over a bare busy/idle signal, which
    /// could otherwise race a slightly-stale poll. Only re-derives peek (and
    /// only touches `selected`'s downstream state, never `selected` itself)
    /// when the flipped row is the currently-selected one.
    fn flip_dashboard_row_status(&mut self, session_id: &str, status: DashboardStatus) {
        let Some(idx) = self
            .dashboard
            .rows
            .iter()
            .position(|r| r.session.id == session_id)
        else {
            return;
        };
        if matches!(
            self.dashboard.rows[idx].status,
            DashboardStatus::AwaitingPermission(_) | DashboardStatus::AwaitingQuestion(_)
        ) {
            return;
        }
        self.dashboard.rows[idx].status = status;
        if idx == self.dashboard.selected {
            self.dashboard.peek = self.dashboard.derive_peek();
        }
    }

    #[must_use]
    pub fn picker_len(&self) -> usize {
        match self.overlay {
            Overlay::Sessions => self.sessions.len(),
            Overlay::Models => self.models.len(),
            Overlay::Agents => self.agents.len(),
            Overlay::Palette(_) => 0,
            Overlay::Files(_) => 0,
            _ => 0,
        }
    }

    pub fn picker_move(&mut self, delta: isize) {
        let len = self.picker_len();
        if len == 0 {
            return;
        }
        let max = len - 1;
        let next = (self.selected as isize + delta).clamp(0, max as isize);
        self.selected = next as usize;
    }

    pub fn picker_confirm(&mut self) -> Option<Msg> {
        let out = match self.overlay {
            Overlay::Models => {
                if let Some(m) = self.models.get(self.selected) {
                    self.model = Some(m.id());
                }
                None
            }
            Overlay::Agents => {
                if let Some(a) = self.agents.get(self.selected) {
                    self.agent = Some(a.name.clone());
                }
                None
            }
            Overlay::Sessions => self
                .sessions
                .get(self.selected)
                .map(|s| Msg::SwitchSession(s.id.clone())),
            _ => None,
        };
        self.close_overlay();
        out
    }

    pub fn open_palette(&mut self) {
        self.overlay = Overlay::Palette(PaletteState::default());
    }

    pub fn palette_input(&mut self, c: char) {
        if let Overlay::Palette(ps) = &mut self.overlay {
            ps.query.push(c);
            ps.selected = 0;
        }
    }

    pub fn palette_backspace(&mut self) {
        if let Overlay::Palette(ps) = &mut self.overlay {
            ps.query.pop();
            ps.selected = 0;
        }
    }

    pub fn palette_move(&mut self, delta: i32) {
        if let Overlay::Palette(ps) = &mut self.overlay {
            let len = palette_matches(&ps.query).len();
            if len == 0 {
                ps.selected = 0;
                return;
            }
            let max = (len - 1) as i32;
            ps.selected = (ps.selected as i32 + delta).clamp(0, max) as usize;
        }
    }

    pub fn palette_confirm(&mut self) -> Option<Msg> {
        let idx = match &self.overlay {
            Overlay::Palette(ps) => *palette_matches(&ps.query).get(ps.selected)?,
            _ => return None,
        };
        match COMMANDS[idx].2 {
            Command::NewSession => {
                self.close_overlay();
                Some(Msg::NewSession)
            }
            Command::ToggleTool => {
                self.close_overlay();
                Some(Msg::ToggleTool)
            }
            Command::Quit => Some(Msg::Quit),
            Command::SwitchSession => {
                self.open_picker(Overlay::Sessions);
                None
            }
            Command::Dashboard => {
                self.open_dashboard();
                None
            }
            Command::ChangeModel => {
                self.open_picker(Overlay::Models);
                None
            }
            Command::ChangeAgent => {
                self.open_picker(Overlay::Agents);
                None
            }
            Command::Help => {
                self.overlay = Overlay::Help;
                None
            }
            Command::AttachFile => {
                self.open_file_picker();
                None
            }
            Command::WorkflowSdd => {
                self.open_text_input("SDD plan file (path)", "sdd");
                None
            }
            Command::WorkflowPlan => {
                self.open_text_input("Plan file (path)", "plan");
                None
            }
            Command::WorkflowTdd => {
                self.open_text_input("TDD feature (describe it)", "tdd");
                None
            }
        }
    }

    /// Toggle the workflow status panel. Opens `Overlay::WorkflowStatus` when a
    /// workflow view exists (guarding the render, which can then assume `Some`);
    /// closes it if already open; a no-op when there is no workflow to show.
    pub fn toggle_workflow_status(&mut self) {
        if matches!(self.overlay, Overlay::WorkflowStatus) {
            self.overlay = Overlay::None;
        } else if self.workflow.is_some() {
            self.overlay = Overlay::WorkflowStatus;
        }
    }

    /// Open the free-text input overlay tagged with a workflow `kind`.
    pub fn open_text_input(&mut self, title: &str, kind: &str) {
        self.overlay = Overlay::TextInput(TextInputState {
            title: title.to_string(),
            query: String::new(),
            kind: kind.to_string(),
            mention: None,
        });
    }

    /// Append `c` to the free-text input query (if that overlay is open). An
    /// `@` typed at a word boundary (query empty or ending in whitespace) opens
    /// an inline mention (kicking off a `/file/list` fetch); otherwise a
    /// keystroke under an active mention just resets its selection.
    pub fn text_input_char(&mut self, c: char) {
        if let Overlay::TextInput(s) = &mut self.overlay {
            let at_boundary = s.query.is_empty() || s.query.ends_with(char::is_whitespace);
            s.query.push(c);
            if c == '@' && at_boundary {
                s.mention = Some(TextMentionState {
                    anchor: s.query.len() - c.len_utf8(),
                    selected: 0,
                    results: Vec::new(),
                    truncated: false,
                    loading: true,
                });
            } else if let Some(m) = &mut s.mention {
                m.selected = 0;
            }
        }
    }

    /// Drop the last character of the free-text input query. Dismisses an
    /// active mention if the `@` anchor was deleted, else resets its selection.
    pub fn text_input_backspace(&mut self) {
        if let Overlay::TextInput(s) = &mut self.overlay {
            s.query.pop();
            let dismiss = s
                .mention
                .as_ref()
                .is_some_and(|m| s.query.len() <= m.anchor);
            if dismiss {
                s.mention = None;
            } else if let Some(m) = &mut s.mention {
                m.selected = 0;
            }
        }
    }

    /// Clamp/step the text-input mention selection by `delta` over its live
    /// ranked matches (biased toward `.otto/plans/`, the common workflow arg).
    pub fn text_input_mention_move(&mut self, delta: i32) {
        if let Overlay::TextInput(s) = &mut self.overlay
            && let Some(m) = &mut s.mention
        {
            let query = &s.query[m.anchor + 1..];
            let len = ranked_matches(&m.results, query, Some(".otto/plans/")).len();
            if len == 0 {
                m.selected = 0;
                return;
            }
            let max = (len - 1) as i32;
            m.selected = (m.selected as i32 + delta).clamp(0, max) as usize;
        }
    }

    /// Dismiss the text-input mention (typed text stays; a second Esc then
    /// closes the whole overlay via the normal path).
    pub fn text_input_clear_mention(&mut self) {
        if let Overlay::TextInput(s) = &mut self.overlay {
            s.mention = None;
        }
    }

    /// Accept the highlighted text-input mention candidate. The workflow arg IS
    /// a path, so a file inserts the **bare** path (no `@`) and clears the
    /// mention; a directory inserts `@path/` (keeping the `@` so drill-down
    /// stays live) and keeps the mention active. No match → no-op.
    pub fn text_input_mention_accept(&mut self) {
        let (path, anchor) = match &self.overlay {
            Overlay::TextInput(TextInputState {
                query,
                mention: Some(m),
                ..
            }) => {
                let ranked =
                    ranked_matches(&m.results, &query[m.anchor + 1..], Some(".otto/plans/"));
                match ranked.get(m.selected) {
                    Some(&i) => (m.results[i].clone(), m.anchor),
                    None => return,
                }
            }
            _ => return,
        };
        let is_dir = path.ends_with('/');
        if let Overlay::TextInput(s) = &mut self.overlay {
            if is_dir {
                s.query.replace_range(anchor.., &format!("@{path}"));
                if let Some(m) = &mut s.mention {
                    m.selected = 0;
                }
            } else {
                s.query.replace_range(anchor.., &path);
                s.mention = None;
            }
        }
    }

    /// Take the files to attach to a submission: the ctrl+f `attachments` plus
    /// any `@`-mention paths whose `@path` token still survives in `text`
    /// (deleted mentions drop out), deduped. Both source lists are drained.
    pub(crate) fn take_files_for_submit(&mut self, text: &str) -> Vec<String> {
        let mut out = std::mem::take(&mut self.attachments);
        for p in std::mem::take(&mut self.mention_paths) {
            if text.contains(&format!("@{p}")) && !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// Enter on the free-text input: close the overlay and, if the trimmed text
    /// is non-empty, emit `StartWorkflow` with it. Empty input is a no-op. A
    /// leading `@` is stripped first: Esc-dismissing a mention mid-drill-down
    /// can leave a literal `@path/` prefix in the query, which would
    /// otherwise be sent to the workflow as an unresolved mention token.
    pub fn text_input_confirm(&mut self) -> Option<Msg> {
        if let Overlay::TextInput(s) = &self.overlay {
            let trimmed = s.query.trim();
            let (kind, arg) = (
                s.kind.clone(),
                trimmed.strip_prefix('@').unwrap_or(trimmed).to_string(),
            );
            self.overlay = Overlay::None;
            if arg.is_empty() {
                return None;
            }
            return Some(Msg::StartWorkflow { kind, arg });
        }
        None
    }

    /// Open the transcript-search overlay with a blank query.
    pub fn open_search(&mut self) {
        self.overlay = Overlay::Search(SearchState::default());
    }

    /// Append `c` to the search query and reset the current-match ordinal
    /// back to the first match (a changed pattern invalidates any prior
    /// position).
    pub fn search_input(&mut self, c: char) {
        if let Overlay::Search(s) = &mut self.overlay {
            s.query.push(c);
            s.current = 0;
        }
    }

    /// Drop the last character of the search query, resetting `current` for
    /// the same reason as `search_input`.
    pub fn search_backspace(&mut self) {
        if let Overlay::Search(s) = &mut self.overlay {
            s.query.pop();
            s.current = 0;
        }
    }

    /// Step the current-match ordinal by `delta`, saturating at 0. The upper
    /// bound depends on the live match count, which is only known at render
    /// time (the transcript is rebuilt fresh each frame), so it's clamped
    /// there instead (`view::transcript`/`view::render_search_bar`).
    pub fn search_move(&mut self, delta: i32) {
        if let Overlay::Search(s) = &mut self.overlay {
            s.current = (s.current as i32 + delta).max(0) as usize;
        }
    }

    /// Open the file-attachment picker and kick off a `/file/list` fetch
    /// (the loop is responsible for the HTTP call and routing
    /// `Msg::FilesLoaded` back in).
    pub fn open_file_picker(&mut self) {
        self.overlay = Overlay::Files(FilePickerState {
            loading: true,
            ..Default::default()
        });
    }

    /// Fold a completed `/file/list` fetch into whichever picker is loading,
    /// resetting the selection to the top of the (now-current) results. The
    /// ctrl+f Files overlay drops directory entries (`ends_with('/')`) — dirs
    /// aren't attachable there, keeping that flow byte-for-byte unchanged; the
    /// `@`-mention pickers keep them for drill-down.
    pub fn files_loaded(&mut self, files: Vec<String>, truncated: bool) {
        match &mut self.overlay {
            Overlay::Files(s) => {
                s.results = files.into_iter().filter(|p| !p.ends_with('/')).collect();
                s.truncated = truncated;
                s.loading = false;
                s.selected = 0;
            }
            Overlay::Mention(m) => {
                m.results = files;
                m.truncated = truncated;
                m.loading = false;
                m.selected = 0;
            }
            Overlay::TextInput(TextInputState {
                mention: Some(m), ..
            }) => {
                m.results = files;
                m.truncated = truncated;
                m.loading = false;
                m.selected = 0;
            }
            _ => {}
        }
    }

    /// Whether any picker is awaiting a `/file/list` fetch — the trigger the
    /// event loop watches to fire exactly one `list_files` call (see
    /// `route_message`).
    #[must_use]
    pub(crate) fn file_fetch_pending(&self) -> bool {
        match &self.overlay {
            Overlay::Files(s) => s.loading,
            Overlay::Mention(m) => m.loading,
            Overlay::TextInput(s) => s.mention.as_ref().is_some_and(|m| m.loading),
            _ => false,
        }
    }

    /// Derive the live `@`-mention query from the editor buffer: the text
    /// between the `@` anchor and the cursor on the anchor row. Returns `None`
    /// when the cursor has left the token (different row, or at/left of the
    /// `@`), which callers treat as "dismiss". Never stored — the buffer is the
    /// single source of truth.
    #[must_use]
    pub(crate) fn mention_query(&self) -> Option<String> {
        let Overlay::Mention(m) = &self.overlay else {
            return None;
        };
        let (row, col) = self.input.cursor();
        if row != m.anchor_row || col <= m.anchor_col {
            return None;
        }
        // `anchor_col` is the byte offset of the `@` (one byte); the query is
        // everything up to the cursor.
        let line = &self.input.lines()[row];
        let start = m.anchor_col + 1;
        (col >= start).then(|| line[start..col].to_string())
    }

    /// Open the inline `@`-mention completion. The `@` has just been inserted,
    /// so the anchor is the cursor position minus that one byte. Kicks off a
    /// `/file/list` fetch (loop-driven, like the ctrl+f picker).
    pub fn open_mention(&mut self) {
        let (row, col) = self.input.cursor();
        self.overlay = Overlay::Mention(MentionState {
            anchor_row: row,
            anchor_col: col.saturating_sub(1),
            selected: 0,
            results: Vec::new(),
            truncated: false,
            loading: true,
        });
    }

    /// Clamp/step the mention selection by `delta` over the live ranked matches.
    pub fn mention_move(&mut self, delta: i32) {
        let query = self.mention_query();
        if let Overlay::Mention(m) = &mut self.overlay {
            let len = query
                .map(|q| ranked_matches(&m.results, &q, None).len())
                .unwrap_or(0);
            if len == 0 {
                m.selected = 0;
                return;
            }
            let max = (len - 1) as i32;
            m.selected = (m.selected as i32 + delta).clamp(0, max) as usize;
        }
    }

    /// React to a buffer edit under an open mention: dismiss if the query is
    /// gone (cursor left the token), else reset the selection to the top.
    pub fn mention_after_edit(&mut self) {
        if self.mention_query().is_none() {
            self.close_overlay();
        } else if let Overlay::Mention(m) = &mut self.overlay {
            m.selected = 0;
        }
    }

    /// Accept the highlighted mention candidate. A file swaps `@partial` →
    /// `@path ` (trailing space), records the path, and closes the overlay. A
    /// directory swaps `@partial` → `@path/` and keeps the overlay open so the
    /// derived query becomes `path/` (fuzzy drill-down over full paths). No
    /// match → dismiss only (never submits — that is the input layer's job).
    pub fn mention_accept(&mut self) {
        let Some(query) = self.mention_query() else {
            self.close_overlay();
            return;
        };
        let (path, anchor_col) = match &self.overlay {
            Overlay::Mention(m) => {
                let ranked = ranked_matches(&m.results, &query, None);
                match ranked.get(m.selected) {
                    Some(&i) => (m.results[i].clone(), m.anchor_col),
                    // No match: dismiss, leaving the typed text in place.
                    None => {
                        self.close_overlay();
                        return;
                    }
                }
            }
            _ => return,
        };
        let is_dir = path.ends_with('/');
        let (row, _) = self.input.cursor();
        // A dir already carries its trailing `/`; a file gets a trailing space
        // so the token is delimited and the next keystroke types normally.
        let replacement = if is_dir {
            format!("@{path}")
        } else {
            format!("@{path} ")
        };
        self.input.replace_to_cursor(row, anchor_col, &replacement);
        if is_dir {
            if let Overlay::Mention(m) = &mut self.overlay {
                m.selected = 0;
            }
        } else {
            if !self.mention_paths.contains(&path) {
                self.mention_paths.push(path);
            }
            self.close_overlay();
        }
    }

    pub fn file_input(&mut self, c: char) {
        if let Overlay::Files(s) = &mut self.overlay {
            s.query.push(c);
            s.selected = 0;
        }
    }

    pub fn file_backspace(&mut self) {
        if let Overlay::Files(s) = &mut self.overlay {
            s.query.pop();
            s.selected = 0;
        }
    }

    pub fn file_move(&mut self, delta: i32) {
        if let Overlay::Files(s) = &mut self.overlay {
            let len = file_matches(&s.results, &s.query).len();
            if len == 0 {
                s.selected = 0;
                return;
            }
            let max = (len - 1) as i32;
            s.selected = (s.selected as i32 + delta).clamp(0, max) as usize;
        }
    }

    /// Toggle the currently-highlighted path in/out of `App.attachments`.
    /// The picker overlay stays open so multiple files can be attached in
    /// one pass.
    pub fn file_toggle(&mut self) {
        let path = match &self.overlay {
            Overlay::Files(s) => {
                let m = file_matches(&s.results, &s.query);
                match m.get(s.selected) {
                    Some(&i) => s.results[i].clone(),
                    None => return,
                }
            }
            _ => return,
        };
        if let Some(pos) = self.attachments.iter().position(|p| p == &path) {
            self.attachments.remove(pos);
        } else {
            self.attachments.push(path);
            self.flash("attached");
        }
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Server(ev) => self.fold_event(ev),
            Msg::Event(ServerEvent::PermissionAsked(p)) => {
                // Only surface the ask if it belongs to the attached session.
                // An ask for a different (backgrounded) session is durable
                // server-side pending state (visible via `GET /permission`) —
                // the dashboard's own poll (Task 5/8) picks it up, so
                // dropping the live event here is safe, not a lost ask.
                if self.session_id.as_deref() == Some(p.session_id.as_str()) {
                    self.overlay = Overlay::Permission(p);
                }
            }
            Msg::Event(ServerEvent::QuestionAsked(q)) => {
                if self.session_id.as_deref() == Some(q.session_id.as_str()) {
                    self.overlay = Overlay::Question(QuestionSession::new(q));
                }
            }
            Msg::Event(ServerEvent::Workflow(w)) => {
                // Fold the same events into the live `WorkflowView` (task→state
                // map) that powers the progress panel + cancel key. Progress and
                // Done are session-guarded so a stale event from an older run
                // cannot corrupt a newer view; a fresh Started resets the view.
                match w.phase {
                    WfPhase::Started => {
                        self.workflow = Some(WorkflowView {
                            kind: w.kind.clone(),
                            arg: w.arg.clone().unwrap_or_default(),
                            session: w.session.clone(),
                            tasks: BTreeMap::new(),
                            done: None,
                        });
                        // A new workflow run means a new `workflow_root`
                        // session (and its first `workflow_task` children)
                        // exist server-side that the dashboard's current
                        // `rows` doesn't know about yet — flag a refetch
                        // (see `DashboardState::needs_refetch`) rather than
                        // waiting out the ~2s poll cadence, but only if the
                        // dashboard is actually open to see it.
                        if matches!(self.overlay, Overlay::Dashboard) {
                            self.dashboard.needs_refetch = true;
                        }
                    }
                    WfPhase::Progress => {
                        if let Some(v) = &mut self.workflow
                            && v.session == w.session
                            && let Some(i) = w.task_index
                        {
                            v.tasks
                                .insert(i, (w.status.clone().unwrap_or_default(), w.notes.clone()));
                        }
                    }
                    WfPhase::Done => {
                        if let Some(v) = &mut self.workflow
                            && v.session == w.session
                        {
                            v.done = Some(w.ok == Some(true));
                        }
                    }
                }
                // Fold each workflow lifecycle event into the transcript so the
                // user watches the run. Must precede the `Msg::Event(_)` wildcard.
                let line = match w.phase {
                    WfPhase::Started => format!(
                        "▶ workflow {} started{}",
                        w.kind,
                        w.arg.map(|a| format!(" ({a})")).unwrap_or_default()
                    ),
                    WfPhase::Progress => format!(
                        "  task {}: {} — {}",
                        w.task_index.unwrap_or(0),
                        w.status.as_deref().unwrap_or("?"),
                        w.notes
                    ),
                    WfPhase::Done => {
                        if w.ok == Some(true) {
                            format!(
                                "✔ workflow {} complete: {}",
                                w.kind,
                                w.summary.as_deref().unwrap_or("")
                            )
                        } else {
                            format!(
                                "✖ workflow {} failed: {}",
                                w.kind,
                                w.error.as_deref().unwrap_or("error")
                            )
                        }
                    }
                };
                self.transcript.push(TranscriptItem::Workflow(line));
                self.bump_render();
            }
            Msg::Event(ServerEvent::Subagent(m)) => {
                // Only for the active run (session-guarded), so stale-run lines
                // don't leak. Must precede the `Msg::Event(_)` wildcard.
                if self
                    .workflow
                    .as_ref()
                    .is_some_and(|w| w.session == m.session)
                {
                    let line = if m.verb == "text" {
                        format!("task {} · \"{}\"", m.task_index, m.detail)
                    } else {
                        format!("task {} ▸ {}: {}", m.task_index, m.verb, m.detail)
                    };
                    self.transcript.push(TranscriptItem::Workflow(line));
                    self.bump_render();
                }
            }
            // Push payoff: flip the matching dashboard row's status in place
            // rather than waiting for the next ~2s poll. `flip_dashboard_row_status`
            // no-ops if the session isn't a dashboard row (not open, or a
            // filtered-out/child row), and never clobbers a pending ask.
            Msg::Event(ServerEvent::SessionBusy { session_id }) => {
                self.flip_dashboard_row_status(&session_id, DashboardStatus::Busy);
            }
            Msg::Event(ServerEvent::SessionIdle { session_id }) => {
                self.flip_dashboard_row_status(&session_id, DashboardStatus::Idle);
            }
            Msg::Event(ServerEvent::SessionCreated { .. }) => {
                // A brand-new session (subagent, workflow child, or a plain
                // top-level one from elsewhere) exists that the dashboard's
                // current `rows` doesn't know about — flag a refetch (see
                // `DashboardState::needs_refetch`) only if the dashboard is
                // open to see it.
                if matches!(self.overlay, Overlay::Dashboard) {
                    self.dashboard.needs_refetch = true;
                }
            }
            Msg::Event(_) => {}
            Msg::SessionsLoaded(s) => self.sessions = s,
            Msg::AgentsLoaded(a) => self.agents = a,
            Msg::ModelsLoaded(m) => self.models = m,
            Msg::HistoryLoaded(rows) => {
                self.load_history(rows);
                self.status = "ready".into();
            }
            Msg::DashboardLoaded {
                sessions,
                permissions,
                questions,
            } => {
                let prev = self
                    .dashboard
                    .rows
                    .get(self.dashboard.selected)
                    .map(|r| (r.session.id.clone(), std::mem::discriminant(&r.status)));
                self.dashboard.rows = apply_pin_and_filter(
                    build_dashboard_rows(
                        &sessions,
                        &permissions,
                        &questions,
                        self.session_id.as_deref(),
                    ),
                    &self.dashboard.pinned,
                    &self.dashboard.filter,
                );
                // A fresh poll landing satisfies any pending push-triggered
                // refetch request (`session.created`/workflow-started while
                // the dashboard was open — see the `Msg::Event` arms below).
                self.dashboard.needs_refetch = false;
                let same_id = prev.as_ref().and_then(|(id, _)| {
                    self.dashboard.rows.iter().position(|r| &r.session.id == id)
                });
                self.dashboard.selected = same_id.unwrap_or_else(|| {
                    self.dashboard
                        .selected
                        .min(self.dashboard.rows.len().saturating_sub(1))
                });
                // Only re-derive the peek if the selected row's identity or
                // status *kind* actually changed — otherwise a still-idle
                // row's freshly-loaded `Message` peek would flicker back to
                // `Loading` on every ~2s poll for no reason.
                let status_changed = same_id.is_none()
                    || prev.is_some_and(|(_, kind)| {
                        self.dashboard
                            .rows
                            .get(self.dashboard.selected)
                            .is_some_and(|r| std::mem::discriminant(&r.status) != kind)
                    });
                if status_changed {
                    self.dashboard.peek = self.dashboard.derive_peek();
                }
            }
            Msg::DashboardPeekLoaded { session_id, text } => {
                if self
                    .dashboard
                    .rows
                    .get(self.dashboard.selected)
                    .is_some_and(|r| r.session.id == session_id)
                {
                    self.dashboard.peek = DashboardPeek::Message(text);
                }
            }
            Msg::Submitted(text) => {
                self.transcript.push(TranscriptItem::User(text));
                self.open_text = None;
                self.open_reasoning = None;
                self.tool_cursor = None;
                self.running_tools.clear();
                self.msg_start = None;
                self.bump_render();
            }
            Msg::Error(e) => self.record_error(e),
            Msg::PromptEnded => {
                // The turn is over — a stale backoff countdown must not keep
                // rewriting the header from the tick handler.
                self.retry = None;
                // Unstick any lingering busy status (thinking/retrying) when the
                // stream ends without a terminal finish. An already-resolved
                // status ("ready", "error: …") is not busy, so it is preserved.
                if self.is_busy() {
                    self.status = "ready".into();
                }
            }
            Msg::Quit => self.should_quit = true,
            // Key/Resize are handled in input.rs (Task 6); ignore here.
            Msg::Key(_) | Msg::Resize => {}
            // The loop performs the HTTP call; here we optimistically mark
            // the matching dashboard row Idle (if the dashboard is tracking
            // it) — the next poll confirms it either way.
            Msg::PermissionReply { id, .. } => {
                if let Some(idx) = self.dashboard.rows.iter().position(
                    |r| matches!(&r.status, DashboardStatus::AwaitingPermission(p) if p.id == id),
                ) {
                    self.dashboard.rows[idx].status = DashboardStatus::Idle;
                    // The peek panel was showing `DashboardPeek::Permission` for
                    // this row (if it's the selected one) — that variant is now
                    // stale (the row is `Idle`), so re-derive it. `derive_peek`
                    // maps `Idle` to `Loading`, which sets up the async fetch
                    // (`maybe_fetch_dashboard_peek` in lib.rs) to resolve the
                    // real latest-message text.
                    if idx == self.dashboard.selected {
                        self.dashboard.peek = self.dashboard.derive_peek();
                    }
                }
            }
            Msg::QuestionReply { id, .. } => {
                if let Some(idx) = self.dashboard.rows.iter().position(
                    |r| matches!(&r.status, DashboardStatus::AwaitingQuestion(q) if q.id == id),
                ) {
                    self.dashboard.rows[idx].status = DashboardStatus::Idle;
                    // See the matching comment in `PermissionReply` above.
                    if idx == self.dashboard.selected {
                        self.dashboard.peek = self.dashboard.derive_peek();
                    }
                }
            }
            // The loop performs the session switch (reload history); no state update here.
            Msg::SwitchSession(_) => {}
            // The loop creates the session then routes a SwitchSession.
            Msg::NewSession => {}
            Msg::ToggleTool => self.toggle_selected_or_last_tool(),
            Msg::ScrollUp => self.scroll_up(3),
            Msg::ScrollDown => self.scroll_down(3),
            Msg::ScrollBottom => self.scroll_to_bottom(),
            Msg::Tick => {
                self.tick = self.tick.wrapping_add(1);
                // Count the startup splash down; it auto-dismisses at zero.
                if let Some(n) = self.splash {
                    self.splash = (n > 1).then_some(n - 1);
                }
                if let Some(f) = &self.flash
                    && self.tick >= f.expires_tick
                {
                    self.flash = None;
                }
                if self.is_busy() {
                    self.spinner_frame = self.spinner_frame.wrapping_add(1);
                    self.busy_ticks = self.busy_ticks.saturating_add(1);
                } else {
                    self.busy_ticks = 0;
                }
                // Re-render the live retry-backoff countdown into the header.
                if let Some(r) = &self.retry {
                    let remaining_ticks = r.expires_tick.saturating_sub(self.tick);
                    if remaining_ticks == 0 {
                        // Backoff elapsed but the next attempt hasn't emitted
                        // an event yet — count UP so a hung reconnect reads as
                        // "still trying for Ns", not a frozen "(now)".
                        let overdue = self
                            .tick
                            .wrapping_sub(r.expires_tick)
                            .div_ceil(crate::view::TICKS_PER_SEC.max(1));
                        if overdue == 0 {
                            self.status = format!("{} (now)", r.prefix);
                        } else {
                            self.status = format!("{} (now +{overdue}s)", r.prefix);
                        }
                    } else {
                        let secs = remaining_ticks.div_ceil(crate::view::TICKS_PER_SEC);
                        self.status = format!("{} ({secs}s)", r.prefix);
                    }
                }
            }
            Msg::FilesLoaded(files, truncated) => {
                self.files_loaded(files, truncated);
            }
            Msg::ToggleTodos => self.todos_collapsed = !self.todos_collapsed,
            // The launch is performed by `dispatch` (lib.rs) as a detached
            // `client.workflow` call; progress folds in via the `/event` pump.
            // Fold a launch line so the user sees the request land immediately.
            Msg::StartWorkflow { kind, .. } => {
                self.transcript.push(TranscriptItem::Workflow(format!(
                    "… launching {kind} workflow"
                )));
                self.bump_render();
            }
            // The cancel HTTP call is performed by `dispatch` (lib.rs); the
            // effect returns as a `workflow.done{ok:false}` event that folds
            // normally. No local state change here.
            Msg::CancelWorkflow(_) => {}
            // The cancel HTTP call is performed by `dispatch` (lib.rs); the run
            // aborting surfaces through the normal stream settle. No local state
            // change here.
            Msg::InterruptTurn(_) => {}
            // `dispatch` (lib.rs) sets `permission_mode` optimistically and
            // performs the HTTP call; no local state change needed here.
            Msg::CyclePermissionMode => {}
            Msg::PermissionModeChanged(mode) => self.permission_mode = mode,
            Msg::FocusChanged(focused) => {
                self.focused = focused;
                if focused && self.title_active {
                    self.title_active = false;
                    self.pending_action = Some(LoopAction::ResetTitle);
                }
            }
            Msg::OsThemeChanged(mode) => {
                if let Some(mode) = mode
                    && self.theme_mode != Some(mode)
                {
                    self.theme_mode = Some(mode);
                    self.theme = match mode {
                        crate::appearance::ThemeMode::Light => self.light_theme.clone(),
                        crate::appearance::ThemeMode::Dark => self.dark_theme.clone(),
                    };
                    self.bump_render();
                    self.pending_action = Some(LoopAction::CursorColor);
                }
            }
            // The HTTP `client.create_session` call is performed by
            // `dispatch` (lib.rs, Task 7), which then routes a
            // `DashboardSessionCreated` on success. Here we only end the
            // "new session" text-entry mode the submit came from — Task 6's
            // key handling is the one place that put us in `NewSession(_)`.
            Msg::CreateDashboardSession(_) => {
                self.dashboard.mode = DashboardMode::Browsing;
            }
            Msg::DashboardSessionCreated(session) => {
                let sid = session.id.clone();
                self.dashboard.rows.push(DashboardRow {
                    session,
                    status: DashboardStatus::Idle,
                    indent: false,
                });
                self.dashboard.rows = apply_pin_and_filter(
                    std::mem::take(&mut self.dashboard.rows),
                    &self.dashboard.pinned,
                    &self.dashboard.filter,
                );
                self.dashboard.selected = self
                    .dashboard
                    .rows
                    .iter()
                    .position(|r| r.session.id == sid)
                    .unwrap_or_else(|| {
                        self.dashboard
                            .selected
                            .min(self.dashboard.rows.len().saturating_sub(1))
                    });
                self.dashboard.peek = self.dashboard.derive_peek();
            }
            Msg::DashboardTogglePin => {
                if let Some(row) = self.dashboard.rows.get(self.dashboard.selected) {
                    let id = row.session.id.clone();
                    if !self.dashboard.pinned.remove(&id) {
                        self.dashboard.pinned.insert(id.clone());
                    }
                    self.dashboard.rows = apply_pin_and_filter(
                        std::mem::take(&mut self.dashboard.rows),
                        &self.dashboard.pinned,
                        &self.dashboard.filter,
                    );
                    // Track the toggled row's session id, not its old index —
                    // pinning just reordered `rows`, so re-find it rather than
                    // leaving `selected` pointing at whatever now sits there.
                    self.dashboard.selected = self
                        .dashboard
                        .rows
                        .iter()
                        .position(|r| r.session.id == id)
                        .unwrap_or(0);
                }
            }
            Msg::DashboardFilterChanged(filter) => {
                self.dashboard.filter = filter;
                let prev_id = self
                    .dashboard
                    .rows
                    .get(self.dashboard.selected)
                    .map(|r| r.session.id.clone());
                self.dashboard.rows = apply_pin_and_filter(
                    std::mem::take(&mut self.dashboard.rows),
                    &self.dashboard.pinned,
                    &self.dashboard.filter,
                );
                let same_id = prev_id
                    .as_ref()
                    .and_then(|id| self.dashboard.rows.iter().position(|r| &r.session.id == id));
                self.dashboard.selected = same_id.unwrap_or(0);
                // Same rationale as `DashboardLoaded`: only re-derive peek
                // when the selected row's identity actually changed, so an
                // already-loaded `Message` peek doesn't flicker back to
                // `Loading` on every keystroke while the selected row stays
                // visible throughout.
                if same_id.is_none() {
                    self.dashboard.peek = self.dashboard.derive_peek();
                }
            }
        }
    }

    /// The text of the newest `TranscriptItem::Assistant` entry, if any.
    /// Backs the OSC-52 yank-last-message key.
    #[must_use]
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Assistant(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// Scroll `n` lines toward older content (away from the bottom), clamped
    /// to the last rendered scroll bound so PageDown always has visible effect.
    pub fn scroll_up(&mut self, n: u32) {
        self.scroll = self
            .scroll
            .saturating_add(n)
            .min(self.last_scroll_max.get());
    }
    /// Scroll `n` lines toward the newest content; 0 = following.
    pub fn scroll_down(&mut self, n: u32) {
        self.scroll = self.scroll.saturating_sub(n);
    }
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.scroll == 0
    }

    /// Whether the todo list has any pending/in-progress item (non-empty and
    /// not all done). Drives whether the todo panel renders (`view::todos_panel`).
    #[must_use]
    pub(crate) fn todos_active(&self) -> bool {
        !self.todos.is_empty()
            && self
                .todos
                .iter()
                .any(|t| matches!(t.status, TodoStatus::Pending | TodoStatus::InProgress))
    }

    /// `(completed count, total count)` across the current todo list.
    #[must_use]
    pub(crate) fn todos_done_total(&self) -> (usize, usize) {
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        (done, self.todos.len())
    }

    /// Flip `expanded` on the most-recently-appended tool row, if any.
    pub fn toggle_last_tool(&mut self) {
        if let Some(TranscriptItem::Tool { expanded, .. }) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|i| matches!(i, TranscriptItem::Tool { .. }))
        {
            *expanded = !*expanded;
            self.bump_render();
        }
    }

    /// Indices of `transcript` items that are tool rows, oldest→newest.
    fn tool_indices(&self) -> Vec<usize> {
        self.transcript
            .iter()
            .enumerate()
            .filter(|(_, it)| matches!(it, TranscriptItem::Tool { .. }))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move the selection to the previous tool row (newest first when starting
    /// from `None`). No-op if there are no tools.
    pub(crate) fn select_prev_tool(&mut self) {
        let tools = self.tool_indices();
        let Some(&last) = tools.last() else { return };
        self.tool_cursor = match self.tool_cursor {
            None => Some(last),
            Some(cur) => {
                let pos = tools.iter().position(|&i| i == cur).unwrap_or(tools.len());
                if pos == 0 {
                    Some(tools[0])
                } else {
                    Some(tools[pos - 1])
                }
            }
        };
    }

    /// Move the selection to the next tool row; stepping past the newest clears
    /// to `None` (resume following the transcript).
    pub(crate) fn select_next_tool(&mut self) {
        let tools = self.tool_indices();
        if tools.is_empty() {
            return;
        }
        self.tool_cursor = match self.tool_cursor {
            None => None,
            Some(cur) => {
                let pos = tools.iter().position(|&i| i == cur).unwrap_or(tools.len());
                tools.get(pos + 1).copied() // None past the newest → follow
            }
        };
    }

    /// Toggle `expanded` on the selected tool, or the newest tool if nothing is
    /// selected (preserving the pre-Phase-21 `t` behavior).
    pub(crate) fn toggle_selected_or_last_tool(&mut self) {
        match self.tool_cursor {
            Some(i) => {
                let flipped = if let Some(TranscriptItem::Tool { expanded, .. }) =
                    self.transcript.get_mut(i)
                {
                    *expanded = !*expanded;
                    true
                } else {
                    false
                };
                if flipped {
                    self.bump_render();
                }
            }
            None => self.toggle_last_tool(),
        }
    }

    /// Fold one streamed `LLMEvent` into the transcript.
    pub fn fold_event(&mut self, ev: LLMEvent) {
        // Any non-Retry stream event means the retried attempt is live again —
        // stop the backoff countdown from rewriting the header.
        if !matches!(ev, LLMEvent::Retry { .. }) {
            self.retry = None;
        }
        match ev {
            LLMEvent::TextStart { .. } => {
                self.mark_msg_start();
                self.transcript
                    .push(TranscriptItem::Assistant(String::new()));
                self.open_text = Some(self.transcript.len() - 1);
                self.bump_render();
            }
            LLMEvent::TextDelta { text, .. } => {
                let idx = self.open_text.unwrap_or_else(|| {
                    self.mark_msg_start();
                    self.transcript
                        .push(TranscriptItem::Assistant(String::new()));
                    let i = self.transcript.len() - 1;
                    self.open_text = Some(i);
                    i
                });
                if let Some(TranscriptItem::Assistant(s)) = self.transcript.get_mut(idx) {
                    s.push_str(&text);
                }
                self.bump_render();
            }
            LLMEvent::TextEnd { .. } => {
                self.open_text = None;
                self.bump_render();
            }
            LLMEvent::ReasoningStart { .. } => {
                self.mark_msg_start();
                self.transcript
                    .push(TranscriptItem::Reasoning(String::new()));
                self.open_reasoning = Some(self.transcript.len() - 1);
                self.bump_render();
            }
            LLMEvent::ReasoningDelta { text, .. } => {
                // Auto-open on an orphan delta (lost/never-sent start frame),
                // mirroring TextDelta — dropping reasoning silently hides work.
                let idx = self.open_reasoning.unwrap_or_else(|| {
                    self.mark_msg_start();
                    self.transcript
                        .push(TranscriptItem::Reasoning(String::new()));
                    let i = self.transcript.len() - 1;
                    self.open_reasoning = Some(i);
                    i
                });
                if let Some(TranscriptItem::Reasoning(s)) = self.transcript.get_mut(idx) {
                    s.push_str(&text);
                }
                self.bump_render();
            }
            LLMEvent::ReasoningEnd { .. } => {
                self.open_reasoning = None;
                self.bump_render();
            }
            LLMEvent::ToolCall {
                id, name, input, ..
            } => {
                if name == "todowrite" && input.get("todos").is_some() {
                    let was_active = self.todos_active();
                    self.todos = parse_todos(&input);
                    if !was_active && self.todos_active() {
                        self.todos_collapsed = false;
                    }
                }
                self.mark_msg_start();
                let title = tool_title(&name, &input);
                self.transcript.push(TranscriptItem::Tool {
                    name,
                    status: ToolStatus::Running,
                    title,
                    input: Some(input),
                    output: None,
                    expanded: false,
                });
                self.running_tools.push((id, self.transcript.len() - 1));
                self.bump_render();
            }
            LLMEvent::ToolResult {
                id, result, output, ..
            } => {
                let text = tool_output_text(&result, &output);
                self.finish_tool(&id, ToolStatus::Ok, text);
            }
            LLMEvent::ToolError { id, message, .. } => {
                self.finish_tool(&id, ToolStatus::Error, Some(message));
            }
            LLMEvent::StepFinish {
                usage: usage @ Some(_),
                ..
            } => {
                self.msg_usage = usage.as_ref().map(usage_in_out);
                self.usage = usage;
            }
            LLMEvent::Finish { usage, reason, .. } => {
                if let Some(u) = usage {
                    self.msg_usage = Some(usage_in_out(&u));
                    self.usage = Some(u);
                }
                // Fold the settled message's usage into the session totals —
                // exactly once, however many step/finish readings mirrored it.
                if let Some((i, o)) = self.msg_usage.take() {
                    self.session_tokens.0 += i;
                    self.session_tokens.1 += o;
                }
                // The assistant message settled — a later Retry belongs to the
                // NEXT message and must not roll this one back.
                self.msg_start = None;
                self.open_text = None;
                self.open_reasoning = None;
                // A turn streams multiple `finish` events over one prompt:
                // each step that requests tools ends with `reason: ToolCalls`,
                // then the tools run and further steps stream before the
                // terminal finish (Stop/Length/…). Only the terminal finish
                // returns to idle — otherwise the header would show "ready"
                // (spinner gone) while tools and later steps are still running.
                if reason != otto_events::FinishReason::ToolCalls {
                    self.status = "ready".into();
                    if !self.focused {
                        self.title_active = true;
                        self.pending_action = Some(LoopAction::Notify);
                    }
                }
            }
            LLMEvent::ProviderError { message, .. } => self.record_error(message),
            LLMEvent::Retry {
                attempt,
                max,
                delay_ms,
                message,
                salvaged,
            } => {
                let secs = delay_ms.div_ceil(1000);
                let label = if is_rate_limit(&message) {
                    "rate-limited — retrying"
                } else {
                    "retrying"
                };
                let prefix = format!("{label} {attempt}/{max}");
                self.status = format!("{prefix} ({secs}s)");
                // Arm the live countdown: each tick re-renders the remaining
                // wait so the header doesn't freeze for the whole backoff.
                let wait_ticks =
                    (delay_ms * u64::from(crate::view::TICKS_PER_SEC)).div_ceil(1000) as u32;
                self.retry = Some(RetryCountdown {
                    prefix,
                    expires_tick: self.tick.wrapping_add(wait_ticks),
                });
                if salvaged {
                    // The failed attempt's completed tool work was KEPT
                    // server-side and the retry continues from it as a new
                    // step — the rendered rows are real; do not roll back.
                    self.msg_start = None;
                    self.open_text = None;
                    self.open_reasoning = None;
                } else if let Some(i) = self.msg_start.take() {
                    // The server purged this attempt's parts and will
                    // re-stream the message from scratch — roll the transcript
                    // back so the partial attempt doesn't remain as an
                    // orphaned duplicate.
                    self.transcript.truncate(i);
                    self.running_tools.retain(|(_, idx)| *idx < i);
                    self.open_text = None;
                    self.open_reasoning = None;
                    self.bump_render();
                }
            }
            LLMEvent::Warning { message } => {
                // Non-fatal quality concern (e.g. a response accepted without
                // finish_reason after retries) — keep it in the scrollback as
                // a dim system line, not an error.
                self.transcript
                    .push(TranscriptItem::Workflow(format!("⚠ {message}")));
                self.bump_render();
            }
            _ => {}
        }
    }

    /// Record a turn-fatal error: append it to the transcript scrollback (so
    /// the reason survives beyond the transient header line) and set the header
    /// status. Clears any open streaming block so a partial answer doesn't look
    /// live.
    fn record_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.transcript.push(TranscriptItem::Error(message.clone()));
        self.status = format!("error: {message}");
        self.retry = None;
        self.msg_start = None;
        self.open_text = None;
        self.open_reasoning = None;
        self.bump_render();
    }

    fn finish_tool(&mut self, id: &str, status: ToolStatus, output: Option<String>) {
        // Match by tool-call id first — parallel tools can finish out of
        // submission order. Fall back to the most recent still-Running row for
        // results whose call was never registered (e.g. missed start frame).
        let row = match self.running_tools.iter().position(|(i, _)| i == id) {
            Some(pos) => {
                let (_, idx) = self.running_tools.remove(pos);
                match self.transcript.get_mut(idx) {
                    Some(item @ TranscriptItem::Tool { .. }) => Some(item),
                    _ => None,
                }
            }
            None => self.transcript.iter_mut().rev().find(|i| {
                matches!(
                    i,
                    TranscriptItem::Tool {
                        status: ToolStatus::Running,
                        ..
                    }
                )
            }),
        };
        if let Some(TranscriptItem::Tool {
            status: s,
            output: o,
            ..
        }) = row
        {
            *s = status;
            if o.is_none() {
                *o = output;
            }
            self.bump_render();
        }
    }

    /// A compact usage summary, if any tokens have been reported.
    ///
    /// Always shows the token count; appends `$cost` when the current model
    /// is resolvable in the embedded registry and carries cost metadata
    /// (many models — especially unknown/future ids — won't, so that
    /// segment degrades gracefully rather than failing the whole line).
    /// Costs below [`MIN_DISPLAYED_COST`] are omitted rather than shown as
    /// the misleading `$0.000`. Context-window usage is reported separately
    /// via [`App::context_pct`].
    #[must_use]
    pub fn usage_line(&self) -> Option<String> {
        let u = self.usage.as_ref()?;
        let total = u
            .total_tokens
            .or_else(|| match (u.input_tokens, u.output_tokens) {
                (Some(i), Some(o)) => Some(i + o),
                _ => None,
            })?;
        let mut parts = vec![format!("{} tok", fmt_tokens(total))];

        if let Some(model_id) = &self.model {
            let r = otto_agent::ModelRef::parse(model_id);
            let m = otto_llm::registry::model_or_default(&r.provider.0, &r.model.0, &r.provider.0);

            if let (Some(cost), Some(inp), Some(out)) =
                (m.cost.as_ref(), u.input_tokens, u.output_tokens)
                && let (Some(ci), Some(co)) = (cost.input, cost.output)
            {
                let total_cost = inp as f64 / 1_000_000.0 * ci + out as f64 / 1_000_000.0 * co;
                // Below this, `{:.3}` rounds to `$0.000` — worse than useless
                // (looks like a real, confirmed-free result). Omit instead.
                if total_cost >= MIN_DISPLAYED_COST {
                    parts.push(format!("${total_cost:.3}"));
                }
            }
        }

        // Real measured session total (Σ input+output across all settled
        // messages) — the number to compare across runs, e.g. tersemode
        // on vs off.
        let session_total = self.session_tokens.0 + self.session_tokens.1;
        if session_total > 0 {
            parts.push(format!("Σ {}", fmt_tokens(session_total)));
        }

        Some(parts.join(" · "))
    }

    /// Context-window usage as a whole percent (0–100), if the current model
    /// resolves to a positive context limit in the embedded registry. Split
    /// from `usage_line` so the view can threshold-color it independently.
    #[must_use]
    pub fn context_pct(&self) -> Option<u8> {
        let u = self.usage.as_ref()?;
        let total = u
            .total_tokens
            .or_else(|| match (u.input_tokens, u.output_tokens) {
                (Some(i), Some(o)) => Some(i + o),
                _ => None,
            })?;
        let model_id = self.model.as_ref()?;
        let r = otto_agent::ModelRef::parse(model_id);
        let m = otto_llm::registry::model_or_default(&r.provider.0, &r.model.0, &r.provider.0);
        let limit = m.limits.context.filter(|c| *c > 0)?;
        let used = u.input_tokens.unwrap_or(total);
        Some((used * 100 / limit).min(100) as u8)
    }

    /// Reset per-session counters when adopting a different session (switch /
    /// new). NOT called on a same-session history reconcile, which must keep
    /// the running totals.
    pub fn reset_session_counters(&mut self) {
        self.session_tokens = (0, 0);
        self.msg_usage = None;
        self.usage = None;
    }

    fn load_history(&mut self, rows: Vec<serde_json::Value>) {
        self.tool_cursor = None;
        self.transcript.clear();
        self.running_tools.clear();
        self.msg_start = None;
        // The open streaming indices point into the transcript we just
        // cleared; left stale, a reconcile racing a live stream makes the
        // next delta append into an arbitrary historical item.
        self.open_text = None;
        self.open_reasoning = None;
        self.todos = Vec::new();
        self.todos_collapsed = false;
        for row in &rows {
            let role = row
                .get("info")
                .and_then(|i| i.get("role"))
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let parts = row
                .get("parts")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            for part in parts {
                let kind = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match (role, kind) {
                    (_, "text") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if role == "user" {
                                self.transcript.push(TranscriptItem::User(t.to_string()));
                            } else {
                                self.transcript
                                    .push(TranscriptItem::Assistant(t.to_string()));
                            }
                        }
                    }
                    (_, "reasoning") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            self.transcript
                                .push(TranscriptItem::Reasoning(t.to_string()));
                        }
                    }
                    (_, "tool") => {
                        let name = part
                            .get("tool")
                            .and_then(|t| t.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let state = part.get("state");
                        let status = state.and_then(|s| s.get("status")).and_then(|s| s.as_str());
                        let status = match status {
                            Some("error") => ToolStatus::Error,
                            Some("completed") => ToolStatus::Ok,
                            Some("running") | Some("pending") => ToolStatus::Running,
                            _ => ToolStatus::Ok,
                        };
                        let input = state.and_then(|s| s.get("input")).cloned();
                        if name == "todowrite"
                            && let Some(i) = &input
                            && i.get("todos").is_some()
                        {
                            self.todos = parse_todos(i);
                        }
                        let output = state
                            .and_then(|s| s.get("output"))
                            .and_then(|o| o.as_str())
                            .map(str::to_string)
                            .or_else(|| {
                                state
                                    .and_then(|s| s.get("error"))
                                    .and_then(|e| e.as_str())
                                    .map(str::to_string)
                            });
                        let title = input
                            .as_ref()
                            .map_or_else(|| name.clone(), |i| tool_title(&name, i));
                        self.transcript.push(TranscriptItem::Tool {
                            name,
                            status,
                            title,
                            input,
                            output,
                            expanded: false,
                        });
                    }
                    _ => {}
                }
            }
        }
        self.bump_render();
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Command {
    NewSession,
    SwitchSession,
    Dashboard,
    ChangeModel,
    ChangeAgent,
    ToggleTool,
    Help,
    Quit,
    AttachFile,
    WorkflowSdd,
    WorkflowPlan,
    WorkflowTdd,
}

/// Source of truth for palette entries: `(label, key_hint, command)`. Labels
/// ending in `…` open an existing picker (Sessions/Models/Agents) instead of
/// acting directly. `key_hint` mirrors the real binding in `input.rs` — display
/// only, no behavior.
pub(crate) const COMMANDS: &[(&str, &str, Command)] = &[
    ("New session", "ctrl+n", Command::NewSession),
    ("Switch session…", "", Command::SwitchSession),
    ("Dashboard…", "", Command::Dashboard),
    ("Change model…", "", Command::ChangeModel),
    ("Change agent…", "ctrl+g", Command::ChangeAgent),
    ("Toggle tool detail", "ctrl+t", Command::ToggleTool),
    ("Help", "?", Command::Help),
    ("Quit", "ctrl+c", Command::Quit),
    ("Attach file…", "ctrl+f", Command::AttachFile),
    (
        "Workflow: SDD…",
        "run subagent-driven dev on a plan file",
        Command::WorkflowSdd,
    ),
    (
        "Workflow: Plan…",
        "execute a plan file with verification",
        Command::WorkflowPlan,
    ),
    (
        "Workflow: TDD…",
        "drive a TDD cycle for a feature",
        Command::WorkflowTdd,
    ),
];

/// Indices into `COMMANDS` that fuzzy-match `query`, best score first,
/// registry order for ties. Empty query returns every index in order.
pub(crate) fn palette_matches(query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, (label, _, _))| fuzzy_subsequence(query, label).map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Case-insensitive subsequence match. `None` if `query`'s chars do not appear
/// in order within `cand`. Higher score = better (bonuses for consecutive runs
/// and matches at word starts).
fn fuzzy_subsequence(query: &str, cand: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = cand.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut prev: Option<usize> = None;
    for (ci, &cc) in cand.iter().enumerate() {
        if qi < q.len() && cc == q[qi] {
            score += 1;
            if matches!(prev, Some(p) if p + 1 == ci) {
                score += 10; // consecutive run
            }
            if ci == 0 || cand[ci - 1] == ' ' {
                score += 10; // word start
            }
            prev = Some(ci);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

/// Indices into `results` that fuzzy-match `query`, best score first,
/// original-order for ties. Empty query returns every index in order.
/// `boost_prefix`, when set, adds a flat +100 to any result whose path starts
/// with it, floating that subtree to the top (e.g. `.otto/plans/` for the SDD
/// workflow arg).
pub(crate) fn ranked_matches(
    results: &[String],
    query: &str,
    boost_prefix: Option<&str>,
) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            fuzzy_subsequence(query, r).map(|s| {
                let boost = boost_prefix
                    .filter(|p| r.starts_with(*p))
                    .map_or(0, |_| 100);
                (i, s + boost)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Unbiased [`ranked_matches`] — the ctrl+f file picker's ranking.
pub(crate) fn file_matches(results: &[String], query: &str) -> Vec<usize> {
    ranked_matches(results, query, None)
}

/// Best-effort display text for a finished tool call.
fn tool_output_text(
    result: &otto_events::ToolResultValue,
    output: &Option<otto_events::ToolOutput>,
) -> Option<String> {
    use otto_events::ToolResultValue as V;
    let from_result = match result {
        V::Text { value } | V::Json { value } | V::Error { value } => match value {
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        },
        V::Content { value } => {
            let joined = value
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
    };
    from_result.or_else(|| {
        output
            .as_ref()
            .map(|o| o.structured.to_string())
            .filter(|s| s != "null")
    })
}

/// A short human title for a tool call (e.g. `read src/main.rs`).
fn tool_title(name: &str, input: &serde_json::Value) -> String {
    let arg = input
        .get("path")
        .or_else(|| input.get("filePath"))
        .or_else(|| input.get("command"))
        .or_else(|| input.get("pattern"))
        .and_then(|v| v.as_str());
    match arg {
        Some(a) => format!("{name} {a}"),
        None => name.to_string(),
    }
}

/// Whether an LLM error message reads as a rate-limit / throttle.
fn is_rate_limit(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("429")
        || m.contains("rate limit")
        || m.contains("too many requests")
        || m.contains("overloaded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn enter_submits_and_clears_input() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        for c in "hello".chars() {
            app.input.insert(c);
        }
        let msg = app.on_key(key(KeyCode::Enter));
        assert!(matches!(msg, Some(Msg::Submitted(s)) if s == "hello"));
        assert!(app.input.is_empty());
    }

    #[test]
    fn question_mark_opens_help_when_input_empty() {
        let mut app = App::new();
        let msg = app.on_key(key(KeyCode::Char('?')));
        assert!(msg.is_none());
        assert!(matches!(app.overlay, Overlay::Help));
    }

    #[test]
    fn esc_closes_overlay() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn ctrl_n_starts_new_session() {
        let mut app = App::new();
        // Fires even with text in the buffer (it's a global chord).
        app.input.insert('h');
        let msg = app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(matches!(msg, Some(Msg::NewSession)));
    }

    #[test]
    fn ctrl_k_opens_palette_when_no_overlay() {
        let mut app = App::new();
        app.on_key(ctrl_key(KeyCode::Char('k')));
        assert!(matches!(app.overlay, Overlay::Palette(_)));
    }

    #[test]
    fn ctrl_k_ignored_while_overlay_open() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        app.on_key(ctrl_key(KeyCode::Char('k')));
        assert!(
            matches!(app.overlay, Overlay::Help),
            "must not hijack open overlay"
        );
    }

    #[test]
    fn ctrl_f_opens_file_picker_when_idle() {
        let mut app = App::new();
        app.on_key(ctrl_key(KeyCode::Char('f')));
        assert!(matches!(app.overlay, Overlay::Files(_)));
    }

    #[test]
    fn ctrl_f_ignored_while_overlay_open() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        app.on_key(ctrl_key(KeyCode::Char('f')));
        assert!(matches!(app.overlay, Overlay::Help));
    }

    fn running_workflow() -> WorkflowView {
        WorkflowView {
            kind: "sdd".into(),
            arg: "p.md".into(),
            session: "ses_9".into(),
            tasks: Default::default(),
            done: None,
        }
    }

    #[test]
    fn ctrl_x_cancels_active_workflow() {
        let mut app = App::new();
        app.workflow = Some(running_workflow());
        let msg = app.on_key(ctrl_key(KeyCode::Char('x')));
        assert!(matches!(msg, Some(Msg::CancelWorkflow(s)) if s == "ses_9"));
    }

    #[test]
    fn ctrl_x_noop_when_no_active_workflow() {
        // No workflow at all → no cancel.
        let mut app = App::new();
        app.workflow = None;
        let msg = app.on_key(ctrl_key(KeyCode::Char('x')));
        assert!(!matches!(msg, Some(Msg::CancelWorkflow(_))));
        // A finished run (`done` set) is also inactive → no cancel.
        let mut done = running_workflow();
        done.done = Some(true);
        app.workflow = Some(done);
        let msg = app.on_key(ctrl_key(KeyCode::Char('x')));
        assert!(!matches!(msg, Some(Msg::CancelWorkflow(_))));
    }

    #[test]
    fn toggle_opens_workflow_status_overlay() {
        let mut app = App::new();
        app.workflow = Some(running_workflow());
        app.toggle_workflow_status();
        assert!(matches!(app.overlay, Overlay::WorkflowStatus));
        // Toggling again closes it.
        app.toggle_workflow_status();
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn toggle_workflow_status_noop_without_workflow() {
        let mut app = App::new();
        app.workflow = None;
        app.toggle_workflow_status();
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn permission_mode_changed_sets_permission_mode() {
        let mut app = App::new();
        assert_eq!(app.permission_mode, "approve-each");
        app.update(Msg::PermissionModeChanged("full-auto".into()));
        assert_eq!(app.permission_mode, "full-auto");
    }

    #[test]
    fn focus_changed_updates_focused_flag() {
        let mut app = App::new();
        assert!(app.focused, "starts focused");
        app.update(Msg::FocusChanged(false));
        assert!(!app.focused);
        app.update(Msg::FocusChanged(true));
        assert!(app.focused);
    }

    #[test]
    fn focus_gained_resets_active_title() {
        let mut app = App::new();
        app.focused = false;
        app.title_active = true;
        app.update(Msg::FocusChanged(true));
        assert!(!app.title_active);
        assert_eq!(app.pending_action, Some(LoopAction::ResetTitle));
    }

    #[test]
    fn focus_gained_is_a_noop_when_no_title_is_active() {
        let mut app = App::new();
        app.focused = false;
        app.update(Msg::FocusChanged(true));
        assert_eq!(app.pending_action, None);
    }

    #[test]
    fn finish_while_unfocused_sets_notify_action() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.focused = false;
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        });
        assert_eq!(app.pending_action, Some(LoopAction::Notify));
        assert!(app.title_active);
    }

    #[test]
    fn finish_while_focused_does_not_notify() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.focused = true;
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        });
        assert_eq!(app.pending_action, None);
        assert!(!app.title_active);
    }

    #[test]
    fn tool_calls_finish_does_not_notify_even_when_unfocused() {
        // A `ToolCalls` finish is a mid-turn step, not the terminal finish —
        // must not fire a premature "turn finished" notification.
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.focused = false;
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        });
        assert_eq!(app.pending_action, None);
    }

    #[test]
    fn os_theme_changed_swaps_theme_and_dedupes() {
        let mut app = App::new();
        app.dark_theme = crate::theme::Theme::dark();
        app.light_theme = crate::theme::Theme::preset("light");
        let gen_before = app.render_gen;

        app.update(Msg::OsThemeChanged(Some(
            crate::appearance::ThemeMode::Light,
        )));
        assert_eq!(app.theme_mode, Some(crate::appearance::ThemeMode::Light));
        assert_eq!(app.theme.accent.fg, app.light_theme.accent.fg);
        assert_eq!(app.pending_action, Some(LoopAction::CursorColor));
        assert!(
            app.render_gen > gen_before,
            "theme swap must bump render_gen"
        );

        // A repeated poll reporting the SAME mode must not re-fire the
        // action or bump render_gen again.
        app.pending_action = None;
        let gen_after_first = app.render_gen;
        app.update(Msg::OsThemeChanged(Some(
            crate::appearance::ThemeMode::Light,
        )));
        assert_eq!(app.pending_action, None, "no change — no action");
        assert_eq!(app.render_gen, gen_after_first);
    }

    #[test]
    fn os_theme_changed_none_is_a_noop() {
        let mut app = App::new();
        let gen_before = app.render_gen;
        app.update(Msg::OsThemeChanged(None));
        assert_eq!(app.theme_mode, None);
        assert_eq!(app.pending_action, None);
        assert_eq!(app.render_gen, gen_before);
    }

    #[test]
    fn file_picker_type_toggle_and_esc() {
        let mut app = App::new();
        app.on_key(ctrl_key(KeyCode::Char('f')));
        app.files_loaded(vec!["Cargo.toml".into(), "src.rs".into()], false);
        for c in "car".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter)); // toggle top match (Cargo.toml)
        assert_eq!(app.attachments, vec!["Cargo.toml".to_string()]);
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn palette_type_and_enter_dispatches() {
        let mut app = App::new();
        app.on_key(ctrl_key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('w')));
        let msg = app.on_key(key(KeyCode::Enter));
        assert!(matches!(msg, Some(Msg::NewSession)));
    }

    #[test]
    fn palette_esc_closes() {
        let mut app = App::new();
        app.open_palette();
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn palette_backspace_removes_last_char() {
        let mut app = App::new();
        app.on_key(ctrl_key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Backspace));
        match &app.overlay {
            Overlay::Palette(ps) => assert_eq!(ps.query, "n", "backspace drops last char"),
            other => panic!("expected palette overlay, got {other:?}"),
        }
    }

    #[test]
    fn open_search_sets_overlay_and_close_overlay_clears_it() {
        let mut app = App::new();
        app.open_search();
        assert!(matches!(app.overlay, Overlay::Search(_)));
        app.close_overlay();
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn search_input_and_backspace_mutate_query_and_reset_current() {
        let mut app = App::new();
        app.open_search();
        app.search_input('h');
        app.search_input('i');
        app.search_move(3); // move off 0 so the reset below is observable
        match &app.overlay {
            Overlay::Search(s) => assert_eq!(s.current, 3),
            other => panic!("expected search overlay, got {other:?}"),
        }
        app.search_input('!');
        match &app.overlay {
            Overlay::Search(s) => {
                assert_eq!(s.query, "hi!");
                assert_eq!(s.current, 0, "typing resets the match ordinal");
            }
            other => panic!("expected search overlay, got {other:?}"),
        }
        app.search_move(2);
        app.search_backspace();
        match &app.overlay {
            Overlay::Search(s) => {
                assert_eq!(s.query, "hi");
                assert_eq!(s.current, 0, "backspace resets the match ordinal");
            }
            other => panic!("expected search overlay, got {other:?}"),
        }
    }

    #[test]
    fn search_move_saturates_at_zero() {
        let mut app = App::new();
        app.open_search();
        app.search_move(-5);
        match &app.overlay {
            Overlay::Search(s) => assert_eq!(s.current, 0),
            other => panic!("expected search overlay, got {other:?}"),
        }
        app.search_move(4);
        match &app.overlay {
            Overlay::Search(s) => assert_eq!(s.current, 4),
            other => panic!("expected search overlay, got {other:?}"),
        }
        app.search_move(-2);
        match &app.overlay {
            Overlay::Search(s) => assert_eq!(s.current, 2),
            other => panic!("expected search overlay, got {other:?}"),
        }
    }

    #[test]
    fn search_methods_are_noops_outside_search_overlay() {
        let mut app = App::new();
        app.overlay = Overlay::Help;
        app.search_input('x');
        app.search_backspace();
        app.search_move(1);
        assert!(
            matches!(app.overlay, Overlay::Help),
            "must not hijack open overlay"
        );
    }

    fn td(text: &str) -> LLMEvent {
        LLMEvent::TextDelta {
            id: "t".into(),
            text: text.into(),
            provider_metadata: None,
        }
    }

    #[test]
    fn transcript_mutation_bumps_render_gen() {
        let mut app = App::new();
        let g0 = app.render_gen;
        app.update(Msg::Server(td("hi"))); // a streaming text delta
        assert_ne!(app.render_gen, g0, "assembling content changed");
    }

    /// A history reconcile racing a live stream: the reload must reset the
    /// open streaming indices, or the next delta appends into an arbitrary
    /// historical item of the rebuilt transcript.
    #[test]
    fn history_reload_resets_open_stream_blocks() {
        let mut app = App::new();
        app.update(Msg::Server(td("streamed before reconcile")));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Assistant(_))
        ));

        // Reconcile: history arrives with two finished rows.
        let rows = vec![
            serde_json::json!({
                "info": { "role": "user" },
                "parts": [{ "type": "text", "text": "old user msg" }]
            }),
            serde_json::json!({
                "info": { "role": "assistant" },
                "parts": [{ "type": "text", "text": "old answer" }]
            }),
        ];
        app.update(Msg::HistoryLoaded(rows));
        let len_after_reload = app.transcript.len();

        // The next delta must open a NEW item, not mutate a historical one.
        app.update(Msg::Server(td("fresh delta")));
        assert_eq!(app.transcript.len(), len_after_reload + 1);
        match app.transcript.last() {
            Some(TranscriptItem::Assistant(s)) => assert_eq!(s, "fresh delta"),
            other => panic!("expected a fresh assistant item, got {other:?}"),
        }
        match &app.transcript[len_after_reload - 1] {
            TranscriptItem::Assistant(s) => {
                assert_eq!(s, "old answer", "historical item untouched");
            }
            other => panic!("unexpected reloaded item {other:?}"),
        }
    }

    #[test]
    fn enter_mid_turn_keeps_input_and_flashes() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.status = "thinking".into(); // turn_in_flight() == true
        for c in "queued while busy".chars() {
            app.input.insert(c);
        }
        let msg = app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(msg.is_none(), "no second concurrent prompt stream");
        assert!(
            !app.input.is_empty(),
            "typed text stays in the editor for after the turn"
        );
        assert!(app.flash.is_some(), "the user is told why nothing happened");

        // Once the turn ends, the same Enter submits.
        app.status = "ready".into();
        let msg = app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(msg, Some(Msg::Submitted(_))));
    }

    #[test]
    fn workflow_events_fold_into_transcript() {
        use crate::sse::{ServerEvent, WfPhase, WorkflowMsg};
        let mut app = App::new();
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Started,
            session: "s".into(),
            kind: "sdd".into(),
            arg: Some("p.md".into()),
            task_index: None,
            status: None,
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Progress,
            session: "s".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: Some(1),
            status: Some("DONE".into()),
            notes: "review clean".into(),
            ok: None,
            summary: None,
            error: None,
        })));
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Done,
            session: "s".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: Some(true),
            summary: Some("2 task(s) processed".into()),
            error: None,
        })));
        let lines: Vec<&String> = app
            .transcript
            .iter()
            .filter_map(|it| match it {
                TranscriptItem::Workflow(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("sdd") && l.contains("started"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("task 1") && l.contains("DONE"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("complete") && l.contains("2 task"))
        );
    }

    #[test]
    fn subagent_activity_folds_task_prefixed_line() {
        use crate::sse::{ServerEvent, SubagentMsg, WfPhase, WorkflowMsg};
        let mut app = App::new();
        // A run must be active + session-matched.
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Started,
            session: "s".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        app.update(Msg::Event(ServerEvent::Subagent(SubagentMsg {
            session: "s".into(),
            task_index: 2,
            verb: "bash".into(),
            detail: "cargo test".into(),
        })));
        let has = app.transcript.iter().any(|it| {
            matches!(it, TranscriptItem::Workflow(s)
                if s.contains("task 2") && s.contains("bash") && s.contains("cargo test"))
        });
        assert!(has);
        // A stale session is ignored.
        app.update(Msg::Event(ServerEvent::Subagent(SubagentMsg {
            session: "OTHER".into(),
            task_index: 9,
            verb: "x".into(),
            detail: "y".into(),
        })));
        assert!(
            !app.transcript
                .iter()
                .any(|it| matches!(it, TranscriptItem::Workflow(s) if s.contains("task 9")))
        );
    }

    #[test]
    fn workflow_view_tracks_task_states() {
        use crate::sse::{ServerEvent, WfPhase, WorkflowMsg};
        let mut app = App::new();
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Started,
            session: "s".into(),
            kind: "sdd".into(),
            arg: Some("p.md".into()),
            task_index: None,
            status: None,
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        assert!(
            app.workflow
                .as_ref()
                .is_some_and(|w| w.done.is_none() && w.kind == "sdd")
        );
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Progress,
            session: "s".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: Some(1),
            status: Some("REVIEWING".into()),
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        assert_eq!(
            app.workflow
                .as_ref()
                .unwrap()
                .tasks
                .get(&1)
                .map(|(s, _)| s.as_str()),
            Some("REVIEWING")
        );
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Done,
            session: "s".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: Some(true),
            summary: Some("2".into()),
            error: None,
        })));
        assert_eq!(app.workflow.as_ref().unwrap().done, Some(true));
    }

    #[test]
    fn workflow_progress_from_stale_session_is_ignored() {
        use crate::sse::{ServerEvent, WfPhase, WorkflowMsg};
        let mut app = App::new();
        // A new run (session "b") is active…
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Started,
            session: "b".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        // …a late Progress from the OLD run (session "a") must not corrupt it.
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Progress,
            session: "a".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: Some(7),
            status: Some("STALE".into()),
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        })));
        assert!(app.workflow.as_ref().unwrap().tasks.is_empty());
        // A stale Done from the old run must not flip the newer view's done.
        app.update(Msg::Event(ServerEvent::Workflow(WorkflowMsg {
            phase: WfPhase::Done,
            session: "a".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: Some(false),
            summary: None,
            error: None,
        })));
        assert_eq!(app.workflow.as_ref().unwrap().done, None);
    }

    #[test]
    fn scroll_and_tick_do_not_bump_render_gen() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Assistant("x".into()));
        let g = app.render_gen;
        app.update(Msg::ScrollUp);
        app.update(Msg::Tick);
        assert_eq!(app.render_gen, g, "view-only messages must not invalidate");
    }

    #[test]
    fn text_deltas_accumulate_into_one_assistant_item() {
        let mut app = App::new();
        app.fold_event(LLMEvent::TextStart {
            id: "t".into(),
            provider_metadata: None,
        });
        app.fold_event(td("hel"));
        app.fold_event(td("lo"));
        app.fold_event(LLMEvent::TextEnd {
            id: "t".into(),
            provider_metadata: None,
        });
        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(&app.transcript[0], TranscriptItem::Assistant(s) if s == "hello"));
    }

    #[test]
    fn tool_call_then_result_transitions_status() {
        let mut app = App::new();
        app.fold_event(LLMEvent::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: serde_json::json!({"path":"a.rs"}),
            provider_executed: None,
            provider_metadata: None,
        });
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::Tool {
                status: ToolStatus::Running,
                ..
            }
        ));
        app.fold_event(LLMEvent::ToolResult {
            id: "c1".into(),
            name: "read".into(),
            // `ToolResultValue` has no `From<&str>`; the brief's real-world
            // constructor for a plain-text result is the `Text` variant.
            result: otto_events::ToolResultValue::Text {
                value: serde_json::json!("ok"),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        });
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::Tool {
                status: ToolStatus::Ok,
                ..
            }
        ));
    }

    #[test]
    fn tool_call_captures_input_and_result_captures_output() {
        let mut app = App::new();
        app.fold_event(LLMEvent::ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            input: serde_json::json!({ "filePath": "a.rs" }),
            provider_executed: None,
            provider_metadata: None,
        });
        match app.transcript.last().unwrap() {
            TranscriptItem::Tool {
                input, expanded, ..
            } => {
                assert_eq!(input.as_ref().unwrap()["filePath"], "a.rs");
                assert!(!expanded, "tools start collapsed");
            }
            _ => panic!("expected a tool item"),
        }
        app.fold_event(LLMEvent::ToolResult {
            id: "c1".into(),
            name: "edit".into(),
            result: otto_events::ToolResultValue::Text {
                value: serde_json::json!("patched a.rs"),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        });
        match app.transcript.last().unwrap() {
            TranscriptItem::Tool { output, status, .. } => {
                assert_eq!(*status, ToolStatus::Ok);
                assert_eq!(output.as_deref(), Some("patched a.rs"));
            }
            _ => panic!("expected a tool item"),
        }
    }

    #[test]
    fn parse_todos_reads_content_and_status() {
        let v = serde_json::json!({ "todos": [
            { "content": "a", "status": "completed" },
            { "content": "b", "status": "in_progress" },
            { "content": "c", "status": "pending" },
            { "content": "d", "status": "cancelled" },
        ]});
        let todos = parse_todos(&v);
        assert_eq!(todos.len(), 4);
        assert_eq!(todos[0].content, "a");
        assert_eq!(todos[0].status, TodoStatus::Completed);
        assert_eq!(todos[1].status, TodoStatus::InProgress);
        assert_eq!(todos[2].status, TodoStatus::Pending);
        assert_eq!(todos[3].status, TodoStatus::Cancelled);
    }

    #[test]
    fn parse_todos_is_defensive() {
        assert!(parse_todos(&serde_json::json!({})).is_empty()); // no todos key
        assert!(parse_todos(&serde_json::json!({ "todos": "nope" })).is_empty()); // not array
        let v = serde_json::json!({ "todos": [ { "status": "weird" } ] }); // missing content, unknown status
        let todos = parse_todos(&v);
        assert_eq!(todos[0].content, "");
        assert_eq!(todos[0].status, TodoStatus::Pending);
    }

    #[test]
    fn todowrite_toolcall_populates_todos_and_empty_clears() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::ToolCall {
            id: "t1".into(),
            name: "todowrite".into(),
            input: serde_json::json!({ "todos": [ { "content": "x", "status": "pending" } ] }),
            provider_executed: None,
            provider_metadata: None,
        });
        assert_eq!(app.todos.len(), 1);
        // an explicit empty list clears
        app.fold_event(otto_events::LLMEvent::ToolCall {
            id: "t2".into(),
            name: "todowrite".into(),
            input: serde_json::json!({ "todos": [] }),
            provider_executed: None,
            provider_metadata: None,
        });
        assert!(app.todos.is_empty());
    }

    #[test]
    fn non_todowrite_toolcall_leaves_todos() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "keep".into(),
            status: TodoStatus::Pending,
        }];
        app.fold_event(otto_events::LLMEvent::ToolCall {
            id: "r1".into(),
            name: "read".into(),
            input: serde_json::json!({ "filePath": "a.rs" }),
            provider_executed: None,
            provider_metadata: None,
        });
        assert_eq!(app.todos.len(), 1, "unrelated tool must not touch todos");
    }

    #[test]
    fn todos_active_and_counts() {
        let mut app = App::new();
        assert!(!app.todos_active());
        app.todos = vec![
            TodoItem {
                content: "a".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "b".into(),
                status: TodoStatus::InProgress,
            },
        ];
        assert!(app.todos_active());
        assert_eq!(app.todos_done_total(), (1, 2));
        app.todos[1].status = TodoStatus::Completed;
        assert!(!app.todos_active(), "all done -> not active");
    }

    #[test]
    fn last_assistant_text_picks_newest() {
        let mut app = App::new();
        assert_eq!(app.last_assistant_text(), None);
        app.transcript
            .push(TranscriptItem::Assistant("first".into()));
        app.transcript.push(TranscriptItem::User("hi".into()));
        app.transcript
            .push(TranscriptItem::Assistant("second".into()));
        assert_eq!(app.last_assistant_text(), Some("second"));
        // A trailing non-Assistant item must not shadow the last Assistant one.
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        assert_eq!(app.last_assistant_text(), Some("second"));
    }

    #[test]
    fn toggle_todos_flips_collapsed() {
        let mut app = App::new();
        assert!(!app.todos_collapsed);
        app.update(Msg::ToggleTodos);
        assert!(app.todos_collapsed);
        app.update(Msg::ToggleTodos);
        assert!(!app.todos_collapsed);
    }

    #[test]
    fn provider_error_sets_status() {
        let mut app = App::new();
        app.fold_event(LLMEvent::TextStart {
            id: "t".into(),
            provider_metadata: None,
        });
        app.fold_event(LLMEvent::ProviderError {
            message: "missing API key".into(),
            classification: None,
            retryable: None,
            provider_metadata: None,
        });
        assert!(
            app.status.starts_with("error:"),
            "status should surface the provider error: {}",
            app.status
        );
        assert!(app.status.contains("missing API key"));
        assert!(app.open_text.is_none());
        assert!(app.open_reasoning.is_none());
        // The error is also recorded in the transcript so it survives the next
        // turn overwriting the header line.
        assert!(
            matches!(app.transcript.last(), Some(TranscriptItem::Error(m)) if m == "missing API key"),
            "provider error recorded in transcript: {:?}",
            app.transcript.last()
        );
    }

    #[test]
    fn msg_error_records_transcript_row_and_status() {
        let mut app = App::new();
        app.update(Msg::Error(
            "lost connection to otto server: timed out".into(),
        ));
        assert!(app.status.starts_with("error:"));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::Error(m)) if m.contains("lost connection")
        ));
    }

    #[test]
    fn prompt_ended_unsticks_retrying_spinner() {
        let mut app = App::new();
        // A retry left the header busy; a subsequent silent stream close must
        // not leave the spinner frozen on "retrying" forever.
        app.status = "retrying 1/5 (2s)".into();
        assert!(app.is_busy(), "retrying is a busy status");
        app.update(Msg::PromptEnded);
        assert_eq!(app.status, "ready");
    }

    #[test]
    fn prompt_ended_unsticks_thinking() {
        let mut app = App::new();
        app.status = "…thinking".into();
        app.update(Msg::PromptEnded);
        assert_eq!(
            app.status, "ready",
            "a silent stream close must clear thinking"
        );
    }

    #[test]
    fn prompt_ended_does_not_clobber_error() {
        let mut app = App::new();
        app.status = "error: boom".into();
        app.update(Msg::PromptEnded);
        assert_eq!(app.status, "error: boom", "a resolved error status wins");
    }

    #[test]
    fn permission_event_opens_and_reply_intent_closes_overlay() {
        let mut app = App::new();
        app.session_id = Some("s".into());
        let asked = PermissionAsked {
            id: "p1".into(),
            session_id: "s".into(),
            permission: "edit".into(),
            patterns: vec![],
        };
        app.update(Msg::Event(ServerEvent::PermissionAsked(asked.clone())));
        assert!(matches!(app.overlay, Overlay::Permission(_)));
        app.close_overlay();
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn permission_asked_for_other_session_does_not_hijack_overlay() {
        let mut app = App::new();
        app.session_id = Some("ses_attached".into());
        app.overlay = Overlay::Sessions;
        app.update(Msg::Event(ServerEvent::PermissionAsked(PermissionAsked {
            id: "perm_1".into(),
            session_id: "ses_other".into(),
            permission: "edit".into(),
            patterns: vec![],
        })));
        assert!(
            matches!(app.overlay, Overlay::Sessions),
            "overlay hijacked by a foreign-session permission ask: {:?}",
            app.overlay
        );
    }

    #[test]
    fn submitted_prompt_appends_user_item() {
        let mut app = App::new();
        app.update(Msg::Submitted("hi there".into()));
        assert!(
            matches!(&app.transcript.last().unwrap(), TranscriptItem::User(s) if s == "hi there")
        );
    }

    #[test]
    fn model_picker_confirm_sets_model() {
        let mut app = App::new();
        app.models = vec![
            crate::client::ModelChoice {
                provider: "anthropic".into(),
                model: "claude-3".into(),
            },
            crate::client::ModelChoice {
                provider: "openai".into(),
                model: "gpt-4".into(),
            },
        ];
        app.open_picker(Overlay::Models);
        assert_eq!(app.selected, 0);
        app.picker_move(1);
        assert_eq!(app.selected, 1);
        assert!(app.picker_confirm().is_none());
        assert_eq!(app.model.as_deref(), Some("openai/gpt-4"));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn agent_picker_confirm_sets_agent() {
        let mut app = App::new();
        app.agents = vec![
            crate::client::AgentInfo {
                name: "build".into(),
            },
            crate::client::AgentInfo {
                name: "plan".into(),
            },
        ];
        app.open_picker(Overlay::Agents);
        app.picker_move(1);
        app.picker_confirm();
        assert_eq!(app.agent.as_deref(), Some("plan"));
    }

    #[test]
    fn session_picker_confirm_emits_switch() {
        let mut app = App::new();
        app.sessions = vec![crate::client::SessionInfo {
            id: "ses_a".into(),
            title: None,
            ..Default::default()
        }];
        app.open_picker(Overlay::Sessions);
        let msg = app.picker_confirm();
        assert!(matches!(msg, Some(Msg::SwitchSession(id)) if id == "ses_a"));
    }

    #[test]
    fn picker_move_clamps_at_bounds() {
        let mut app = App::new();
        app.models = vec![crate::client::ModelChoice {
            provider: "p".into(),
            model: "m".into(),
        }];
        app.open_picker(Overlay::Models);
        app.picker_move(-1);
        assert_eq!(app.selected, 0);
        app.picker_move(5);
        assert_eq!(app.selected, 0); // only one row
    }

    #[test]
    fn toggle_tool_flips_last_tool_expanded() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        app.toggle_last_tool();
        assert!(matches!(
            app.transcript.last().unwrap(),
            TranscriptItem::Tool { expanded: true, .. }
        ));
    }

    #[test]
    fn model_picker_arrows_navigate_and_select() {
        // The bare `m` shortcut was removed (models open via the ctrl+k
        // palette); this exercises the picker's own nav/select behavior.
        let mut app = App::new();
        app.models = vec![
            crate::client::ModelChoice {
                provider: "a".into(),
                model: "x".into(),
            },
            crate::client::ModelChoice {
                provider: "b".into(),
                model: "y".into(),
            },
        ];
        app.open_picker(Overlay::Models);
        assert!(matches!(app.overlay, Overlay::Models));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected, 1);
        let msg = app.on_key(key(KeyCode::Enter));
        assert!(msg.is_none());
        assert_eq!(app.model.as_deref(), Some("b/y"));
    }

    #[test]
    fn scroll_up_moves_away_from_bottom() {
        let mut app = App::new();
        app.last_scroll_max.set(100); // as published by a render
        assert!(app.is_following()); // scroll == 0
        app.scroll_up(3);
        assert_eq!(app.scroll, 3);
        assert!(!app.is_following());
    }

    #[test]
    fn scroll_down_returns_to_follow() {
        let mut app = App::new();
        app.last_scroll_max.set(100);
        app.scroll_up(5);
        app.scroll_down(2);
        assert_eq!(app.scroll, 3);
        app.scroll_down(10); // saturates at 0 = following
        assert_eq!(app.scroll, 0);
        assert!(app.is_following());
    }

    #[test]
    fn scroll_up_clamps_to_last_rendered_max() {
        let mut app = App::new();
        app.last_scroll_max.set(7);
        // Holding PageUp past the top must not build overscroll debt that
        // PageDown then silently unwinds.
        for _ in 0..10 {
            app.scroll_up(3);
        }
        assert_eq!(app.scroll, 7, "clamped at the rendered max");
        app.scroll_down(3);
        assert_eq!(app.scroll, 4, "PageDown moves immediately");
    }

    #[test]
    fn scroll_up_with_no_render_yet_is_inert() {
        let mut app = App::new();
        // Nothing rendered yet (last_scroll_max == 0): nothing to scroll to.
        app.scroll_up(3);
        assert_eq!(app.scroll, 0);
        assert!(app.is_following());
    }

    #[test]
    fn scroll_to_bottom_follows() {
        let mut app = App::new();
        app.last_scroll_max.set(100);
        app.scroll_up(4);
        app.scroll_to_bottom();
        assert_eq!(app.scroll, 0);
        assert!(app.is_following());
    }

    #[test]
    fn is_busy_tracks_thinking_status() {
        let mut app = App::new();
        app.status = "…thinking".into();
        assert!(app.is_busy());
        app.status = "ready".into();
        assert!(!app.is_busy());
        // The momentary startup status should also get the busy/spinner
        // styling so the app never reads as frozen while it connects.
        app.status = "connecting…".into();
        assert!(app.is_busy());
    }

    #[test]
    fn running_tool_returns_newest_running() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: "read a.rs".into(),
            input: None,
            output: None,
            expanded: false,
        });
        assert_eq!(app.running_tool(), None); // no running tool
        app.transcript.push(TranscriptItem::Tool {
            name: "bash".into(),
            status: ToolStatus::Running,
            title: "bash ls -F".into(),
            input: None,
            output: None,
            expanded: false,
        });
        assert_eq!(app.running_tool(), Some(("bash", "bash ls -F")));
    }

    #[test]
    fn tick_advances_spinner_only_when_busy() {
        let mut app = App::new();
        app.status = "…thinking".into();
        app.update(Msg::Tick);
        assert_eq!(app.spinner_frame, 1);
        assert_eq!(app.busy_ticks, 1);
        // Idle tick does not advance.
        app.status = "ready".into();
        app.update(Msg::Tick);
        assert_eq!(app.spinner_frame, 1);
    }

    #[test]
    fn finish_with_toolcalls_stays_busy() {
        // A step that ends by requesting tools is NOT the end of the turn —
        // tools still run and more steps stream over the same prompt. The
        // header must keep animating, not flip to idle "ready".
        let mut app = App::new();
        app.status = "…thinking".into();
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        });
        assert!(
            app.is_busy(),
            "tool-call step finish must stay busy, got status {:?}",
            app.status
        );
        assert_ne!(app.status, "ready");
    }

    #[test]
    fn finish_with_stop_returns_to_ready() {
        let mut app = App::new();
        app.status = "…thinking".into();
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        });
        assert_eq!(app.status, "ready");
    }

    #[test]
    fn finish_captures_usage() {
        let mut app = App::new();
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                total_tokens: Some(1234),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        assert_eq!(app.usage.as_ref().and_then(|u| u.total_tokens), Some(1234));
        let line = app.usage_line().unwrap();
        assert!(line.contains("tok"), "usage line: {line}");
    }

    /// `anthropic/claude-sonnet-4-5` is priced (input $3/M, output $15/M) and
    /// carries a 200k context limit in the embedded models.dev snapshot
    /// (`crates/otto-llm/assets/models.json`), so it exercises both the
    /// cost and ctx% branches of `usage_line`.
    #[test]
    fn usage_line_shows_cost_for_large_usage() {
        let mut app = App::new();
        app.model = Some("anthropic/claude-sonnet-4-5".into());
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(1_000_000),
                output_tokens: Some(1_000_000),
                total_tokens: Some(2_000_000),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        let line = app.usage_line().unwrap();
        assert!(line.contains('$'), "usage line: {line}");
        assert!(!line.contains("$0.000"), "usage line: {line}");
    }

    #[test]
    fn context_pct_reports_percent_for_known_model() {
        let mut app = App::new();
        app.model = Some("anthropic/claude-sonnet-4-5".into());
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(100_000),
                output_tokens: Some(0),
                total_tokens: Some(100_000),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        // 100k of a 200k context ≈ 50%.
        assert_eq!(app.context_pct(), Some(50));
    }

    #[test]
    fn context_pct_none_without_usage_or_model() {
        assert_eq!(App::new().context_pct(), None);
    }

    /// Same priced model, but a tiny usage whose real cost rounds to
    /// `$0.000` at 3 decimals — the segment must be omitted rather than
    /// showing that misleading value.
    #[test]
    fn usage_line_omits_negligible_cost() {
        let mut app = App::new();
        app.model = Some("anthropic/claude-sonnet-4-5".into());
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                total_tokens: Some(2),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        let line = app.usage_line().unwrap();
        assert!(line.contains("tok"), "usage line: {line}");
        assert!(!line.contains("$0.000"), "usage line: {line}");
    }

    /// Session totals accumulate once per assistant message: StepFinish and
    /// the terminal Finish both carry the SAME per-message usage (the
    /// processor replaces, not adds — `a.tokens = tokens`), so folding both
    /// must not double-count.
    #[test]
    fn session_tokens_accumulate_per_message_not_per_event() {
        let mut app = App::new();
        // Message 1: step usage then a mirroring finish.
        app.fold_event(LLMEvent::StepFinish {
            index: 0,
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::ToolCalls,
            usage: Some(otto_events::Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        assert_eq!(app.session_tokens, (100, 50), "one message, counted once");

        // Message 2: usage only on the terminal finish.
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(200),
                output_tokens: Some(80),
                total_tokens: Some(280),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        assert_eq!(
            app.session_tokens,
            (300, 130),
            "totals accumulate across messages"
        );
    }

    /// A finish with no usage must not re-add the PREVIOUS message's usage.
    #[test]
    fn usage_free_finish_does_not_recount_previous_message() {
        let mut app = App::new();
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: Some(otto_events::Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            provider_metadata: None,
        });
        app.fold_event(LLMEvent::Finish {
            reason: otto_events::FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        });
        assert_eq!(app.session_tokens, (100, 50));
    }

    #[test]
    fn usage_line_includes_session_total() {
        let mut app = App::new();
        for _ in 0..2 {
            app.fold_event(LLMEvent::Finish {
                reason: otto_events::FinishReason::Stop,
                usage: Some(otto_events::Usage {
                    input_tokens: Some(600),
                    output_tokens: Some(400),
                    total_tokens: Some(1000),
                    ..Default::default()
                }),
                provider_metadata: None,
            });
        }
        let line = app.usage_line().unwrap();
        assert!(
            line.contains("Σ 2.0k"),
            "session total rendered, got {line:?}"
        );
    }

    #[test]
    fn leaving_busy_resets_elapsed() {
        let mut app = App::new();
        app.status = "…thinking".into();
        app.update(Msg::Tick);
        app.update(Msg::Tick);
        assert_eq!(app.busy_ticks, 2);
        app.status = "ready".into();
        app.update(Msg::Tick); // first idle tick resets
        assert_eq!(app.busy_ticks, 0);
    }

    #[test]
    fn fuzzy_matches_subsequence_case_insensitive() {
        assert!(fuzzy_subsequence("cm", "Change model…").is_some());
        assert!(fuzzy_subsequence("CM", "Change model…").is_some());
        assert!(fuzzy_subsequence("xyz", "Change model…").is_none());
        assert_eq!(fuzzy_subsequence("", "anything"), Some(0));
    }

    #[test]
    fn fuzzy_scores_word_starts_and_runs_higher() {
        // "cm" hits two word-starts in "Change model" -> beats a scattered match
        let strong = fuzzy_subsequence("cm", "Change model…").unwrap();
        let weak = fuzzy_subsequence("cm", "Toggle tool detailcm-noword").unwrap_or(0);
        assert!(strong > weak, "word-start match should score higher");
        // consecutive run beats gapped: "new" contiguous in "New session"
        assert!(
            fuzzy_subsequence("new", "New session").unwrap()
                > fuzzy_subsequence("nsn", "New session").unwrap()
        );
    }

    #[test]
    fn palette_matches_empty_returns_all_in_order() {
        let all = palette_matches("");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(all, (0..COMMANDS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn palette_matches_ranks_change_model_first_for_cm() {
        let m = palette_matches("cm");
        assert!(!m.is_empty());
        assert_eq!(COMMANDS[m[0]].2, Command::ChangeModel);
    }

    #[test]
    fn palette_matches_filters_out_non_matches() {
        let m = palette_matches("zzzz");
        assert!(m.is_empty());
    }

    #[test]
    fn open_palette_sets_overlay_and_empty_query() {
        let mut app = App::new();
        app.open_palette();
        match &app.overlay {
            Overlay::Palette(ps) => {
                assert_eq!(ps.query, "");
                assert_eq!(ps.selected, 0);
            }
            _ => panic!("expected palette overlay"),
        }
    }

    #[test]
    fn palette_typing_filters_and_resets_selection() {
        let mut app = App::new();
        app.open_palette();
        app.palette_move(1); // selected -> 1
        app.palette_input('n');
        app.palette_input('e');
        app.palette_input('w');
        match &app.overlay {
            Overlay::Palette(ps) => {
                assert_eq!(ps.query, "new");
                assert_eq!(ps.selected, 0, "typing resets selection");
            }
            _ => panic!("expected palette overlay"),
        }
        // "New session" is a match
        assert!(
            palette_matches("new")
                .iter()
                .any(|&i| COMMANDS[i].2 == Command::NewSession)
        );
    }

    #[test]
    fn palette_confirm_new_session_returns_msg() {
        let mut app = App::new();
        app.open_palette();
        for c in "new".chars() {
            app.palette_input(c);
        }
        let msg = app.palette_confirm();
        assert!(matches!(msg, Some(Msg::NewSession)));
        assert!(matches!(app.overlay, Overlay::None), "palette closed");
    }

    #[test]
    fn palette_confirm_change_model_opens_model_picker() {
        let mut app = App::new();
        app.open_palette();
        for c in "cm".chars() {
            app.palette_input(c);
        }
        let msg = app.palette_confirm();
        assert!(msg.is_none());
        assert!(
            matches!(app.overlay, Overlay::Models),
            "opened model picker"
        );
    }

    #[test]
    fn palette_attach_file_command_opens_picker() {
        let mut app = App::new();
        app.open_palette();
        for c in "attach".chars() {
            app.palette_input(c);
        }
        let msg = app.palette_confirm();
        assert!(msg.is_none());
        assert!(matches!(app.overlay, Overlay::Files(_)));
    }

    #[test]
    fn palette_move_clamps_to_match_count() {
        let mut app = App::new();
        app.open_palette();
        app.palette_move(-1); // cannot go below 0
        if let Overlay::Palette(ps) = &app.overlay {
            assert_eq!(ps.selected, 0);
        }
        app.palette_move(1000); // clamp to last match
        if let Overlay::Palette(ps) = &app.overlay {
            assert_eq!(ps.selected, COMMANDS.len() - 1);
        }
    }

    #[test]
    fn open_file_picker_sets_loading_overlay() {
        let mut app = App::new();
        app.open_file_picker();
        match &app.overlay {
            Overlay::Files(s) => {
                assert!(s.loading);
                assert!(s.results.is_empty());
            }
            _ => panic!("expected files overlay"),
        }
    }

    #[test]
    fn files_loaded_populates_and_clears_loading() {
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["a.rs".into(), "b.txt".into()], false);
        match &app.overlay {
            Overlay::Files(s) => {
                assert!(!s.loading);
                assert_eq!(s.results.len(), 2);
            }
            _ => panic!("expected files overlay"),
        }
    }

    #[test]
    fn palette_workflow_sdd_opens_text_input() {
        let mut app = App::new();
        app.open_palette();
        for c in "workflow sdd".chars() {
            app.palette_input(c);
        }
        let msg = app.palette_confirm();
        assert!(
            matches!(app.overlay, Overlay::TextInput(ref s) if s.kind == "sdd"),
            "expected sdd text-input overlay, got {:?}",
            app.overlay
        );
        assert!(msg.is_none(), "opening the overlay emits no Msg");
    }

    #[test]
    fn text_input_enter_emits_start_workflow() {
        let mut app = App::new();
        app.overlay = Overlay::TextInput(TextInputState {
            title: "SDD plan file".into(),
            query: "docs/plan.md".into(),
            kind: "sdd".into(),
            mention: None,
        });
        let msg = app.text_input_confirm();
        assert!(
            matches!(
                msg,
                Some(Msg::StartWorkflow { ref kind, ref arg })
                    if kind == "sdd" && arg == "docs/plan.md"
            ),
            "expected StartWorkflow{{sdd, docs/plan.md}}, got {msg:?}"
        );
        assert!(matches!(app.overlay, Overlay::None), "overlay closed");
    }

    #[test]
    fn text_input_empty_confirm_is_noop() {
        let mut app = App::new();
        app.overlay = Overlay::TextInput(TextInputState {
            title: "SDD plan file".into(),
            query: "   ".into(),
            kind: "sdd".into(),
            mention: None,
        });
        let msg = app.text_input_confirm();
        assert!(msg.is_none(), "empty/whitespace arg emits no Msg");
        assert!(matches!(app.overlay, Overlay::None), "overlay still closed");
    }

    #[test]
    fn text_input_char_and_backspace_edit_query() {
        let mut app = App::new();
        app.open_text_input("TDD feature", "tdd");
        app.text_input_char('h');
        app.text_input_char('i');
        app.text_input_backspace();
        match &app.overlay {
            Overlay::TextInput(s) => {
                assert_eq!(s.query, "h");
                assert_eq!(s.kind, "tdd");
                assert_eq!(s.title, "TDD feature");
            }
            _ => panic!("expected text-input overlay"),
        }
    }

    #[test]
    fn file_matches_fuzzy_filters() {
        let results = vec!["src/main.rs".to_string(), "Cargo.toml".to_string()];
        let m = file_matches(&results, "cargo");
        assert_eq!(m.len(), 1);
        assert_eq!(results[m[0]], "Cargo.toml");
        assert_eq!(file_matches(&results, "").len(), 2, "empty query = all");
    }

    #[test]
    fn file_toggle_adds_then_removes_path() {
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["a.rs".into(), "b.txt".into()], false);
        app.file_toggle(); // selected=0 -> attach a.rs
        assert_eq!(app.attachments, vec!["a.rs".to_string()]);
        assert!(matches!(app.overlay, Overlay::Files(_)), "stays open");
        app.file_toggle(); // toggle same -> remove
        assert!(app.attachments.is_empty());
    }

    #[test]
    fn attaching_a_file_flashes() {
        // The real API is `file_toggle` (toggles the highlighted match in the
        // open Files picker overlay), not a `toggle_attachment(path)` setter —
        // matched to that shape here (see state.rs `file_toggle`).
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["src/main.rs".into()], false);
        app.file_toggle();
        assert!(app.attachments.iter().any(|p| p == "src/main.rs"));
        assert_eq!(app.flash.as_ref().map(|f| f.msg.as_str()), Some("attached"));
    }

    #[test]
    fn file_move_clamps_to_filtered_len() {
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["a.rs".into(), "b.txt".into(), "c.md".into()], false);
        app.file_move(-1);
        if let Overlay::Files(s) = &app.overlay {
            assert_eq!(s.selected, 0);
        }
        app.file_move(100);
        if let Overlay::Files(s) = &app.overlay {
            assert_eq!(s.selected, 2);
        }
    }

    #[test]
    fn load_history_hydrates_todos_from_last_todowrite() {
        let mut app = App::new();
        // Two message rows: a user text part, then an assistant todowrite tool part.
        let rows = vec![serde_json::json!({
            "info": { "role": "assistant" },
            "parts": [
                { "type": "tool", "state": {
                    "status": "completed",
                    "input": { "todos": [
                        { "content": "one", "status": "completed" },
                        { "content": "two", "status": "in_progress" }
                    ] },
                    "title": "todowrite"
                }, "tool": "todowrite" }
            ]
        })];
        app.load_history(rows);
        assert_eq!(app.todos.len(), 2);
        assert_eq!(app.todos[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn load_history_clears_stale_todos() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "stale".into(),
            status: TodoStatus::Pending,
        }];
        app.todos_collapsed = true;
        app.load_history(vec![]); // switching to a session with no todowrite parts
        assert!(app.todos.is_empty(), "stale todos cleared on load");
        assert!(!app.todos_collapsed, "collapse state reset on load");
    }

    #[test]
    fn todowrite_expands_panel_on_inactive_to_active_transition() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "done".into(),
            status: TodoStatus::Completed,
        }];
        app.todos_collapsed = true;
        app.fold_event(LLMEvent::ToolCall {
            id: "t1".into(),
            name: "todowrite".into(),
            input: serde_json::json!({ "todos": [
                { "content": "done", "status": "completed" },
                { "content": "new", "status": "pending" },
            ] }),
            provider_executed: None,
            provider_metadata: None,
        });
        assert!(
            !app.todos_collapsed,
            "a fresh active list must re-expand a collapsed panel"
        );
    }

    #[test]
    fn todowrite_does_not_reexpand_while_already_active() {
        let mut app = App::new();
        app.todos = vec![TodoItem {
            content: "a".into(),
            status: TodoStatus::InProgress,
        }];
        app.todos_collapsed = true;
        app.fold_event(LLMEvent::ToolCall {
            id: "t1".into(),
            name: "todowrite".into(),
            input: serde_json::json!({ "todos": [
                { "content": "a", "status": "completed" },
                { "content": "b", "status": "in_progress" },
            ] }),
            provider_executed: None,
            provider_metadata: None,
        });
        assert!(
            app.todos_collapsed,
            "an active -> active update must not fight a deliberate user collapse"
        );
    }

    #[test]
    fn ctrl_o_toggles_todos() {
        let mut app = App::new();
        let msg = app.on_key(ctrl_key(KeyCode::Char('o')));
        assert!(matches!(msg, Some(Msg::ToggleTodos)));
    }

    #[test]
    fn o_inserts_char_when_input_nonempty() {
        let mut app = App::new();
        app.input.insert('x');
        let msg = app.on_key(key(KeyCode::Char('o')));
        assert!(msg.is_none());
        assert!(
            app.input.text().contains("xo"),
            "o typed into non-empty input"
        );
    }

    #[test]
    fn retry_event_sets_rate_limited_status() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 2,
            max: 5,
            delay_ms: 8000,
            salvaged: false,
            message: "http error: status 429: rate limit".into(),
        });
        assert_eq!(app.status, "rate-limited — retrying 2/5 (8s)");
        assert!(app.is_busy(), "retry status keeps the spinner animating");
    }

    #[test]
    fn retry_event_non_rate_limit_uses_plain_label() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 1,
            max: 5,
            delay_ms: 2000,
            salvaged: false,
            message: "transport error: connection reset".into(),
        });
        assert_eq!(app.status, "retrying 1/5 (2s)");
        assert!(
            !app.status.starts_with("error"),
            "must not render as a fatal error"
        );
    }

    #[test]
    fn parallel_tools_finish_by_id_not_position() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            provider_executed: None,
            provider_metadata: None,
        });
        app.fold_event(otto_events::LLMEvent::ToolCall {
            id: "call_2".into(),
            name: "edit".into(),
            input: serde_json::json!({}),
            provider_executed: None,
            provider_metadata: None,
        });
        // The OLDER call finishes first (parallel tools complete out of
        // order): its result must land on call_1's row, not the most-recent
        // Running row.
        app.fold_event(otto_events::LLMEvent::ToolResult {
            id: "call_1".into(),
            name: "read".into(),
            result: otto_events::ToolResultValue::Text {
                value: serde_json::json!("done"),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        });
        let tools: Vec<(&str, &ToolStatus)> = app
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Tool { name, status, .. } => Some((name.as_str(), status)),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0],
            ("read", &ToolStatus::Ok),
            "call_1 (read) finished"
        );
        assert_eq!(
            tools[1],
            ("edit", &ToolStatus::Running),
            "call_2 (edit) still running"
        );
    }

    #[test]
    fn reasoning_delta_without_start_opens_block() {
        let mut app = App::new();
        // First frame of the stream was lost (chunk corruption, reconnect):
        // the delta must open a block like TextDelta does, not vanish.
        app.fold_event(otto_events::LLMEvent::ReasoningDelta {
            id: "r1".into(),
            text: "thinking…".into(),
            provider_metadata: None,
        });
        let reasoning = app.transcript.iter().find_map(|i| match i {
            TranscriptItem::Reasoning(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(reasoning.as_deref(), Some("thinking…"));
    }

    #[test]
    fn retry_rolls_back_partial_attempt_no_duplicate_blocks() {
        let mut app = App::new();
        // Attempt 1 streams partial text, then dies mid-stream.
        app.fold_event(otto_events::LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        });
        app.fold_event(otto_events::LLMEvent::TextDelta {
            id: "t1".into(),
            text: "partial answer".into(),
            provider_metadata: None,
        });
        // The server purges the attempt's parts and retries…
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 1,
            max: 5,
            delay_ms: 2000,
            salvaged: false,
            message: "connection reset".into(),
        });
        // …and attempt 2 re-streams the message from scratch.
        app.fold_event(otto_events::LLMEvent::TextStart {
            id: "t2".into(),
            provider_metadata: None,
        });
        app.fold_event(otto_events::LLMEvent::TextDelta {
            id: "t2".into(),
            text: "full answer".into(),
            provider_metadata: None,
        });
        app.fold_event(otto_events::LLMEvent::TextEnd {
            id: "t2".into(),
            provider_metadata: None,
        });

        let assistants: Vec<&str> = app
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Assistant(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistants,
            vec!["full answer"],
            "attempt-1 partial text must not remain as an orphaned duplicate"
        );
    }

    #[test]
    fn retry_countdown_ticks_down_live() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 1,
            max: 5,
            delay_ms: 4000,
            salvaged: false,
            message: "http error: status 429: rate limit".into(),
        });
        assert!(app.status.ends_with("(4s)"), "initial: {}", app.status);
        // One second of ticks (8/s): the header must count down live, not
        // freeze on the snapshot taken when the Retry event arrived.
        for _ in 0..8 {
            app.update(Msg::Tick);
        }
        assert!(
            app.status.ends_with("(3s)"),
            "status must count down live, got {:?}",
            app.status
        );
        // Draining the rest of the wait reaches the expiry, then counts UP —
        // a hung reconnect must read as "still trying for Ns", not a frozen
        // "(now)".
        for _ in 0..40 {
            app.update(Msg::Tick);
        }
        assert!(
            app.status.ends_with("(now +2s)"),
            "overdue countdown counts up, got {:?}",
            app.status
        );
        assert!(app.is_busy(), "overdue retry still animates the spinner");
        for _ in 0..8 {
            app.update(Msg::Tick);
        }
        assert!(
            app.status.ends_with("(now +3s)"),
            "keeps counting up, got {:?}",
            app.status
        );
    }

    #[test]
    fn retry_countdown_cleared_when_prompt_ends() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 1,
            max: 5,
            delay_ms: 8000,
            salvaged: false,
            message: "overloaded".into(),
        });
        app.update(Msg::PromptEnded);
        assert_eq!(app.status, "ready");
        for _ in 0..8 {
            app.update(Msg::Tick);
        }
        assert_eq!(
            app.status, "ready",
            "a stale countdown must not resurrect the retrying status"
        );
    }

    #[test]
    fn retry_countdown_cleared_by_next_stream_event() {
        let mut app = App::new();
        app.fold_event(otto_events::LLMEvent::Retry {
            attempt: 1,
            max: 5,
            delay_ms: 4000,
            salvaged: false,
            message: "overloaded".into(),
        });
        // The retried attempt starts streaming: the countdown must stop
        // rewriting the header.
        app.fold_event(otto_events::LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        });
        app.status = "…thinking".into();
        for _ in 0..8 {
            app.update(Msg::Tick);
        }
        assert_eq!(
            app.status, "…thinking",
            "a stale countdown must not clobber the live status"
        );
    }

    #[test]
    fn every_command_has_a_key_hint() {
        // Switch session / Dashboard / Change model are intentionally
        // palette-only (no real key binding); everything else must have one.
        for (label, key, _cmd) in COMMANDS {
            let palette_only = matches!(*label, "Switch session…" | "Dashboard…" | "Change model…");
            assert_eq!(
                key.is_empty(),
                palette_only,
                "command {label:?} has unexpected key_hint {key:?}"
            );
        }
    }

    #[test]
    fn palette_matches_still_filters_after_tuple_widen() {
        // "quit" should match the Quit entry by subsequence.
        let idx = palette_matches("quit");
        assert!(idx.iter().any(|&i| COMMANDS[i].2 == Command::Quit));
    }

    #[test]
    fn flash_sets_message_with_expiry() {
        let mut app = App::new();
        app.flash("copied");
        let f = app.flash.as_ref().expect("flash set");
        assert_eq!(f.msg, "copied");
        assert_eq!(f.expires_tick, app.tick + FLASH_TICKS);
    }

    #[test]
    fn flash_clears_after_expiry_ticks() {
        let mut app = App::new();
        app.status = "ready".into(); // not busy
        app.flash("sent");
        for _ in 0..FLASH_TICKS {
            app.update(Msg::Tick);
        }
        assert!(
            app.flash.is_none(),
            "flash should clear once tick >= expires_tick"
        );
    }

    #[test]
    fn tick_counter_advances_even_when_idle() {
        let mut app = App::new();
        app.status = "ready".into();
        let before = app.tick;
        app.update(Msg::Tick);
        assert_eq!(app.tick, before + 1);
    }

    #[test]
    fn splash_counts_down_over_ticks_then_clears() {
        let mut app = App::new();
        app.splash = Some(3);
        app.update(Msg::Tick);
        assert_eq!(app.splash, Some(2));
        app.update(Msg::Tick);
        assert_eq!(app.splash, Some(1));
        app.update(Msg::Tick);
        assert_eq!(app.splash, None, "auto-dismissed at zero");
        // Idempotent once cleared — later ticks leave it None.
        app.update(Msg::Tick);
        assert_eq!(app.splash, None);
    }

    #[test]
    fn no_splash_by_default() {
        assert_eq!(App::new().splash, None);
    }

    #[test]
    fn sessions_loaded_replaces_list_for_refreshed_titles() {
        let mut app = App::new();
        app.sessions = vec![crate::client::SessionInfo {
            id: "s1".into(),
            title: Some("New session".into()),
            ..Default::default()
        }];
        // A mid-run refresh (picker reopen) brings the auto-generated title.
        app.update(Msg::SessionsLoaded(vec![crate::client::SessionInfo {
            id: "s1".into(),
            title: Some("Streaming client retry logic".into()),
            ..Default::default()
        }]));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(
            app.sessions[0].title.as_deref(),
            Some("Streaming client retry logic")
        );
    }

    fn push_tool(app: &mut App, title: &str) {
        app.transcript.push(TranscriptItem::Tool {
            name: "read".into(),
            status: ToolStatus::Ok,
            title: title.into(),
            input: None,
            output: None,
            expanded: false,
        });
    }

    #[test]
    fn toggling_selected_tool_bumps_render_gen() {
        let mut app = App::new();
        push_tool(&mut app, "a");
        app.tool_cursor = Some(0);
        let before = app.render_gen;
        app.toggle_selected_or_last_tool();
        assert!(
            app.render_gen > before,
            "toggling a selected tool must bump render_gen so the cache reassembles"
        );
    }

    #[test]
    fn tool_nav_selects_tool_items_only() {
        let mut app = App::new();
        app.transcript.push(TranscriptItem::User("hi".into()));
        push_tool(&mut app, "a"); // index 1
        app.transcript.push(TranscriptItem::Assistant("ok".into()));
        push_tool(&mut app, "b"); // index 3
        assert_eq!(app.tool_cursor, None);
        app.select_prev_tool(); // from None → newest tool (index 3)
        assert_eq!(app.tool_cursor, Some(3));
        app.select_prev_tool(); // → previous tool (index 1)
        assert_eq!(app.tool_cursor, Some(1));
        app.select_next_tool(); // → back to index 3
        assert_eq!(app.tool_cursor, Some(3));
        app.select_next_tool(); // past newest → None (follow)
        assert_eq!(app.tool_cursor, None);
    }

    #[test]
    fn toggle_selected_or_last_flips_selected() {
        let mut app = App::new();
        push_tool(&mut app, "a"); // index 0
        push_tool(&mut app, "b"); // index 1
        app.tool_cursor = Some(0);
        app.toggle_selected_or_last_tool();
        let expanded0 = matches!(
            app.transcript[0],
            TranscriptItem::Tool { expanded: true, .. }
        );
        assert!(expanded0, "selected (index 0) toggled");
        let expanded1 = matches!(
            app.transcript[1],
            TranscriptItem::Tool { expanded: true, .. }
        );
        assert!(!expanded1, "unselected untouched");
    }

    #[test]
    fn toggle_selected_or_last_falls_back_to_last() {
        let mut app = App::new();
        push_tool(&mut app, "a");
        push_tool(&mut app, "b"); // newest, index 1
        app.tool_cursor = None;
        app.toggle_selected_or_last_tool();
        assert!(matches!(
            app.transcript[1],
            TranscriptItem::Tool { expanded: true, .. }
        ));
    }

    #[test]
    fn load_history_clears_tool_cursor() {
        let mut app = App::new();
        push_tool(&mut app, "a");
        app.tool_cursor = Some(0);
        app.load_history(Vec::new()); // reload with empty history
        assert_eq!(
            app.tool_cursor, None,
            "history reload must clear stale selection"
        );
    }

    // ----- Task D: inline `@` file/folder mention -------------------------

    #[test]
    fn ranked_matches_boost_prefers_prefix() {
        let results = vec!["zzz/plan.md".to_string(), ".otto/plans/x.md".to_string()];
        // Both fuzzy-match "plan"; the boost floats the `.otto/plans/` subtree
        // to the top regardless of the base fuzzy score.
        let ranked = ranked_matches(&results, "plan", Some(".otto/plans/"));
        assert_eq!(results[ranked[0]], ".otto/plans/x.md");
        // Unbiased ranking still returns both (no forced ordering here).
        assert_eq!(ranked_matches(&results, "plan", None).len(), 2);
    }

    #[test]
    fn open_mention_captures_anchor_and_loads() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        match &app.overlay {
            Overlay::Mention(m) => {
                assert_eq!((m.anchor_row, m.anchor_col), (0, 0));
                assert!(m.loading, "opens in loading state to trigger the fetch");
            }
            other => panic!("expected mention overlay, got {other:?}"),
        }
        assert_eq!(app.input.text(), "@", "the '@' is inserted into the buffer");
    }

    #[test]
    fn mention_query_derives_from_buffer() {
        let mut app = App::new();
        // A multibyte prefix before the trigger exercises byte-offset slicing.
        for c in "é ".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Char('@')));
        assert!(matches!(app.overlay, Overlay::Mention(_)));
        assert_eq!(app.mention_query().as_deref(), Some(""), "empty at the '@'");
        for c in "wörld".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.mention_query().as_deref(), Some("wörld"));
        assert_eq!(app.input.text(), "é @wörld");
    }

    #[test]
    fn files_loaded_folds_into_mention_and_textinput_mention() {
        // Chat editor mention.
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        app.files_loaded(vec!["a.rs".into(), "src/".into()], true);
        match &app.overlay {
            Overlay::Mention(m) => {
                assert_eq!(m.results, vec!["a.rs".to_string(), "src/".to_string()]);
                assert!(m.truncated);
                assert!(!m.loading);
            }
            other => panic!("expected mention overlay, got {other:?}"),
        }
        // Text-input (workflow arg) mention.
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        app.files_loaded(vec![".otto/plans/p.md".into()], false);
        match &app.overlay {
            Overlay::TextInput(s) => {
                let m = s.mention.as_ref().expect("mention still active");
                assert_eq!(m.results, vec![".otto/plans/p.md".to_string()]);
                assert!(!m.loading);
            }
            other => panic!("expected text-input overlay, got {other:?}"),
        }
    }

    #[test]
    fn files_loaded_filters_dirs_from_ctrl_f_picker() {
        let mut app = App::new();
        app.open_file_picker();
        app.files_loaded(vec!["a.rs".into(), "src/".into(), "b.rs".into()], false);
        match &app.overlay {
            Overlay::Files(s) => assert_eq!(
                s.results,
                vec!["a.rs".to_string(), "b.rs".to_string()],
                "ctrl+f picker drops directory entries"
            ),
            other => panic!("expected files overlay, got {other:?}"),
        }
    }

    #[test]
    fn mention_accept_file_replaces_token_appends_space_and_records_path() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        app.files_loaded(vec!["src/main.rs".into(), "src/".into()], false);
        for c in "main".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter)); // accept highlighted file
        assert_eq!(app.input.text(), "@src/main.rs ", "token swapped + space");
        assert_eq!(app.mention_paths, vec!["src/main.rs".to_string()]);
        assert!(
            matches!(app.overlay, Overlay::None),
            "overlay closed on file"
        );
    }

    #[test]
    fn mention_accept_dir_inserts_trailing_slash_keeps_overlay_no_attach() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        app.files_loaded(vec!["src/".into(), "src/main.rs".into()], false);
        for c in "src".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter)); // accept highlighted dir (src/)
        assert_eq!(app.input.text(), "@src/", "dir keeps its trailing slash");
        assert!(
            matches!(app.overlay, Overlay::Mention(_)),
            "dir accept keeps the dropdown open for drill-down"
        );
        assert!(app.mention_paths.is_empty(), "dirs are not attachable");
        assert_eq!(app.mention_query().as_deref(), Some("src/"));
    }

    #[test]
    fn mention_backspace_past_at_dismisses() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('@')));
        for c in "sr".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace)); // 'r'
        app.on_key(key(KeyCode::Backspace)); // 's'
        assert!(
            matches!(app.overlay, Overlay::Mention(_)),
            "still open at empty query"
        );
        app.on_key(key(KeyCode::Backspace)); // deletes the '@'
        assert!(
            matches!(app.overlay, Overlay::None),
            "dismissed with the '@'"
        );
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn take_files_for_submit_drops_deleted_mentions_keeps_ctrl_f_attachments_and_dedups() {
        let mut app = App::new();
        app.attachments = vec!["ctrlf.rs".into()];
        // "kept.rs" listed twice (dedup) + "gone.rs" whose token was edited out.
        app.mention_paths = vec!["kept.rs".into(), "gone.rs".into(), "kept.rs".into()];
        let files = app.take_files_for_submit("see @kept.rs please");
        assert_eq!(files, vec!["ctrlf.rs".to_string(), "kept.rs".to_string()]);
        assert!(app.attachments.is_empty(), "attachments drained");
        assert!(app.mention_paths.is_empty(), "mention paths drained");
    }

    #[test]
    fn text_input_at_opens_biased_mention() {
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        match &app.overlay {
            Overlay::TextInput(s) => {
                let m = s.mention.as_ref().expect("mention opened");
                assert_eq!(m.anchor, 0);
                assert!(m.loading);
            }
            other => panic!("expected text-input overlay, got {other:?}"),
        }
        app.files_loaded(vec!["src/plan.rs".into(), ".otto/plans/p.md".into()], false);
        let m = match &app.overlay {
            Overlay::TextInput(s) => s.mention.as_ref().unwrap(),
            other => panic!("expected text-input overlay, got {other:?}"),
        };
        // Empty query: the `.otto/plans/` bias floats the plan file to the top.
        let ranked = ranked_matches(&m.results, "", Some(".otto/plans/"));
        assert_eq!(m.results[ranked[0]], ".otto/plans/p.md");
    }

    #[test]
    fn text_input_mention_accept_inserts_bare_path() {
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        app.files_loaded(vec![".otto/plans/p.md".into()], false);
        app.text_input_mention_accept();
        match &app.overlay {
            Overlay::TextInput(s) => {
                assert_eq!(s.query, ".otto/plans/p.md", "bare path, no '@'");
                assert!(s.mention.is_none(), "file accept clears the mention");
            }
            other => panic!("expected text-input overlay, got {other:?}"),
        }
    }

    #[test]
    fn text_input_confirm_after_accept_starts_workflow_with_path() {
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        app.files_loaded(vec![".otto/plans/p.md".into()], false);
        app.text_input_mention_accept();
        let msg = app.text_input_confirm();
        assert!(
            matches!(msg, Some(Msg::StartWorkflow { ref kind, ref arg })
                if kind == "sdd" && arg == ".otto/plans/p.md"),
            "confirm feeds the accepted path to the workflow, got {msg:?}"
        );
    }

    #[test]
    fn text_input_confirm_strips_leftover_at_from_dismissed_mention() {
        // Accepting a DIRECTORY keeps `@dir/` in the query (live drill-down);
        // Esc-dismissing the mention (`text_input_clear_mention`) leaves that
        // literal `@`-prefixed text behind. Confirming from there must not
        // send the raw `@dir/` token to the workflow.
        let mut app = App::new();
        app.open_text_input("SDD plan file", "sdd");
        app.text_input_char('@');
        app.files_loaded(vec![".otto/plans/".into()], false);
        app.text_input_mention_accept();
        match &app.overlay {
            Overlay::TextInput(s) => assert_eq!(s.query, "@.otto/plans/"),
            other => panic!("expected text-input overlay, got {other:?}"),
        }
        app.text_input_clear_mention();
        let msg = app.text_input_confirm();
        assert!(
            matches!(msg, Some(Msg::StartWorkflow { ref kind, ref arg })
                if kind == "sdd" && arg == ".otto/plans/"),
            "leading '@' must be stripped before starting the workflow, got {msg:?}"
        );
    }

    fn sample_question_asked() -> crate::sse::QuestionAsked {
        crate::sse::QuestionAsked {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![
                crate::sse::QuestionPromptView {
                    question: "First?".into(),
                    header: "q1".into(),
                    options: vec![
                        crate::sse::QuestionOptionView {
                            label: "A".into(),
                            description: "a".into(),
                        },
                        crate::sse::QuestionOptionView {
                            label: "B".into(),
                            description: "b".into(),
                        },
                    ],
                    multiple: false,
                },
                crate::sse::QuestionPromptView {
                    question: "Second?".into(),
                    header: "q2".into(),
                    options: vec![
                        crate::sse::QuestionOptionView {
                            label: "X".into(),
                            description: "x".into(),
                        },
                        crate::sse::QuestionOptionView {
                            label: "Y".into(),
                            description: "y".into(),
                        },
                    ],
                    multiple: true,
                },
            ],
        }
    }

    #[test]
    fn question_asked_opens_the_overlay() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.update(Msg::Event(ServerEvent::QuestionAsked(
            sample_question_asked(),
        )));
        assert!(matches!(app.overlay, Overlay::Question(_)));
    }

    #[test]
    fn question_asked_for_other_session_does_not_hijack_overlay() {
        let mut app = App::new();
        app.session_id = Some("ses_attached".into());
        app.overlay = Overlay::Sessions;
        app.update(Msg::Event(ServerEvent::QuestionAsked(
            crate::sse::QuestionAsked {
                id: "que_1".into(),
                session_id: "ses_other".into(),
                questions: vec![crate::sse::QuestionPromptView {
                    question: "Pick one".into(),
                    header: "choice".into(),
                    options: vec![crate::sse::QuestionOptionView {
                        label: "A".into(),
                        description: "a".into(),
                    }],
                    multiple: false,
                }],
            },
        )));
        assert!(
            matches!(app.overlay, Overlay::Sessions),
            "overlay hijacked by a foreign-session question ask: {:?}",
            app.overlay
        );
    }

    #[test]
    fn question_session_toggle_and_advance_single_select() {
        let mut qs = QuestionSession::new(sample_question_asked());
        assert_eq!(qs.current, 0);
        qs.toggle(1); // select option B
        assert_eq!(qs.cursor, vec![1]);
        let done = qs.confirm_current();
        assert!(!done, "one more question remains");
        assert_eq!(qs.current, 1);
        assert_eq!(qs.answers, vec![vec![1]]);
        assert!(qs.cursor.is_empty(), "cursor resets for the next question");
    }

    #[test]
    fn question_session_multi_select_accumulates_and_requires_nonempty() {
        // Answer question 1 first to advance into question 2 (multi-select).
        let mut qs = QuestionSession::new(sample_question_asked());
        qs.toggle(0);
        qs.confirm_current();
        assert_eq!(qs.current, 1);
        // Question 2 is multi-select: toggling twice selects both, confirm with empty cursor should not advance.
        let advanced_empty = qs.confirm_current();
        assert!(
            !advanced_empty,
            "empty multi-select cursor must not confirm"
        );
        qs.toggle(0);
        qs.toggle(1);
        assert_eq!(qs.cursor, vec![0, 1]);
        let done = qs.confirm_current();
        assert!(done, "last question confirmed");
        assert_eq!(qs.answers, vec![vec![0], vec![0, 1]]);
    }

    fn dash_perm(session_id: &str) -> crate::sse::PermissionAsked {
        crate::sse::PermissionAsked {
            id: format!("perm_{session_id}"),
            session_id: session_id.into(),
            permission: "edit".into(),
            patterns: vec!["*.rs".into()],
        }
    }

    fn dash_single_question(session_id: &str) -> crate::sse::QuestionAsked {
        crate::sse::QuestionAsked {
            id: format!("que_{session_id}"),
            session_id: session_id.into(),
            questions: vec![crate::sse::QuestionPromptView {
                question: "Pick one".into(),
                header: "choice".into(),
                options: vec![
                    crate::sse::QuestionOptionView {
                        label: "A".into(),
                        description: "a".into(),
                    },
                    crate::sse::QuestionOptionView {
                        label: "B".into(),
                        description: "b".into(),
                    },
                ],
                multiple: false,
            }],
        }
    }

    fn dash_session(id: &str, updated: i64, busy: bool, parent: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: None,
            time_updated: updated,
            time_created: updated,
            busy,
            parent_id: parent.map(str::to_string),
            kind: None,
        }
    }

    /// Like `dash_session`, but tagged with a `kind` (`"workflow_root"`/
    /// `"workflow_task"`/`"subagent"`) for grouping tests.
    fn dash_session_kind(id: &str, updated: i64, parent: Option<&str>, kind: &str) -> SessionInfo {
        SessionInfo {
            kind: Some(kind.into()),
            ..dash_session(id, updated, false, parent)
        }
    }

    /// Like `dash_session`, but with a `title` set, for filter tests.
    fn dash_session_titled(
        id: &str,
        updated: i64,
        parent: Option<&str>,
        title: &str,
    ) -> SessionInfo {
        SessionInfo {
            title: Some(title.into()),
            ..dash_session(id, updated, false, parent)
        }
    }

    #[test]
    fn build_dashboard_rows_excludes_attached_and_subagent_sessions() {
        let sessions = vec![
            dash_session("ses_attached", 10, false, None),
            dash_session("ses_child", 20, false, Some("ses_attached")),
            dash_session("ses_other", 30, false, None),
        ];
        let rows = build_dashboard_rows(&sessions, &[], &[], Some("ses_attached"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.id, "ses_other");
    }

    #[test]
    fn build_dashboard_rows_sorts_awaiting_before_busy_before_idle() {
        let sessions = vec![
            dash_session("ses_idle", 10, false, None),
            dash_session("ses_busy", 20, true, None),
            dash_session("ses_ask", 5, false, None),
        ];
        let rows = build_dashboard_rows(&sessions, &[dash_perm("ses_ask")], &[], None);
        let order: Vec<&str> = rows.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(order, vec!["ses_ask", "ses_busy", "ses_idle"]);
    }

    #[test]
    fn build_dashboard_rows_ties_break_by_recency() {
        let sessions = vec![
            dash_session("ses_a", 10, true, None),
            dash_session("ses_b", 20, true, None),
        ];
        let rows = build_dashboard_rows(&sessions, &[], &[], None);
        assert_eq!(rows[0].session.id, "ses_b");
    }

    #[test]
    fn build_dashboard_rows_splices_workflow_tasks_after_their_root_indented() {
        let sessions = vec![
            dash_session_kind("root", 10, None, "workflow_root"),
            dash_session_kind("task2", 20, Some("root"), "workflow_task"),
            dash_session_kind("task1", 10, Some("root"), "workflow_task"),
            dash_session("other", 5, false, None),
        ];
        let rows = build_dashboard_rows(&sessions, &[], &[], None);
        let order: Vec<(&str, bool)> = rows
            .iter()
            .map(|r| (r.session.id.as_str(), r.indent))
            .collect();
        // "root" outranks "other" (both Idle -> tie-broken by recency: 10 >
        // 5), children immediately follow it, oldest (task1) first.
        assert_eq!(
            order,
            vec![
                ("root", false),
                ("task1", true),
                ("task2", true),
                ("other", false),
            ]
        );
    }

    #[test]
    fn build_dashboard_rows_drops_orphaned_workflow_task() {
        // "orphan"'s parent_id ("missing") never appears as a primary row
        // (no session with that id at all here) — it must not surface as an
        // indented row with no visible parent.
        let sessions = vec![
            dash_session("other", 5, false, None),
            dash_session_kind("orphan", 20, Some("missing"), "workflow_task"),
        ];
        let rows = build_dashboard_rows(&sessions, &[], &[], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.id, "other");
    }

    #[test]
    fn build_dashboard_rows_still_excludes_subagent_and_kindless_children() {
        let sessions = vec![
            dash_session("root", 10, false, None),
            dash_session_kind("sub", 20, Some("root"), "subagent"),
            dash_session("adhoc_child", 30, false, Some("root")),
        ];
        let rows = build_dashboard_rows(&sessions, &[], &[], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.id, "root");
    }

    #[test]
    fn apply_pin_and_filter_floats_pinned_row_to_top_stably() {
        let rows = vec![
            DashboardRow {
                session: dash_session("a", 30, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 20, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("c", 10, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        let pinned: HashSet<String> = ["c".to_string()].into_iter().collect();
        let out = apply_pin_and_filter(rows, &pinned, "");
        let order: Vec<&str> = out.iter().map(|r| r.session.id.as_str()).collect();
        // "c" floats to the top; "a"/"b" keep their original relative order
        // behind it (not re-sorted by recency or anything else).
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    #[test]
    fn apply_pin_and_filter_child_travels_with_its_pinned_or_unpinned_parent() {
        let rows = vec![
            DashboardRow {
                session: dash_session("root_a", 30, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("child_a", 25, false, Some("root_a")),
                status: DashboardStatus::Idle,
                indent: true,
            },
            DashboardRow {
                session: dash_session("root_b", 20, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("child_b", 15, false, Some("root_b")),
                status: DashboardStatus::Idle,
                indent: true,
            },
        ];
        let pinned: HashSet<String> = ["root_b".to_string()].into_iter().collect();
        let out = apply_pin_and_filter(rows, &pinned, "");
        let order: Vec<&str> = out.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(order, vec!["root_b", "child_b", "root_a", "child_a"]);
    }

    #[test]
    fn apply_pin_and_filter_keeps_parent_whose_child_title_matches() {
        let rows = vec![
            DashboardRow {
                session: dash_session_titled("root", 20, None, "unrelated title"),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session_titled("child", 10, Some("root"), "fix the bug"),
                status: DashboardStatus::Idle,
                indent: true,
            },
        ];
        let out = apply_pin_and_filter(rows, &HashSet::new(), "bug");
        let order: Vec<&str> = out.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["root", "child"],
            "parent survives via matching child, even though its own title doesn't match"
        );
    }

    #[test]
    fn apply_pin_and_filter_hides_non_matching_child_of_a_kept_parent() {
        let rows = vec![
            DashboardRow {
                session: dash_session_titled("root", 20, None, "fix the bug"),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session_titled("child", 10, Some("root"), "unrelated title"),
                status: DashboardStatus::Idle,
                indent: true,
            },
        ];
        let out = apply_pin_and_filter(rows, &HashSet::new(), "bug");
        let order: Vec<&str> = out.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["root"],
            "parent matches and is kept, but its non-matching child stays hidden"
        );
    }

    #[test]
    fn apply_pin_and_filter_drops_filtered_out_parent_and_its_children() {
        let rows = vec![
            DashboardRow {
                session: dash_session_titled("root", 20, None, "nothing relevant"),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session_titled("child", 10, Some("root"), "also nothing relevant"),
                status: DashboardStatus::Idle,
                indent: true,
            },
            DashboardRow {
                session: dash_session_titled("other", 5, None, "bug fix here"),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        let out = apply_pin_and_filter(rows, &HashSet::new(), "bug");
        let order: Vec<&str> = out.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(order, vec!["other"]);
    }

    #[test]
    fn derive_peek_permission_row() {
        let dash = DashboardState {
            rows: vec![DashboardRow {
                session: dash_session("s", 0, false, None),
                status: DashboardStatus::AwaitingPermission(dash_perm("s")),
                indent: false,
            }],
            selected: 0,
            peek: DashboardPeek::Loading,
            ..DashboardState::default()
        };
        assert_eq!(dash.derive_peek(), DashboardPeek::Permission);
    }

    #[test]
    fn derive_peek_single_select_question_row() {
        let dash = DashboardState {
            rows: vec![DashboardRow {
                session: dash_session("s", 0, false, None),
                status: DashboardStatus::AwaitingQuestion(dash_single_question("s")),
                indent: false,
            }],
            selected: 0,
            peek: DashboardPeek::Loading,
            ..DashboardState::default()
        };
        assert_eq!(dash.derive_peek(), DashboardPeek::Question { highlight: 0 });
    }

    #[test]
    fn derive_peek_multi_select_question_needs_full_session() {
        let mut q = dash_single_question("s");
        q.questions[0].multiple = true;
        let dash = DashboardState {
            rows: vec![DashboardRow {
                session: dash_session("s", 0, false, None),
                status: DashboardStatus::AwaitingQuestion(q),
                indent: false,
            }],
            selected: 0,
            peek: DashboardPeek::Loading,
            ..DashboardState::default()
        };
        assert_eq!(dash.derive_peek(), DashboardPeek::NeedsFullSession);
    }

    #[test]
    fn dashboard_loaded_preserves_message_peek_when_status_kind_unchanged() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::Idle,
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Message("hi".into());
        app.update(Msg::DashboardLoaded {
            sessions: vec![dash_session("s", 5, false, None)],
            permissions: vec![],
            questions: vec![],
        });
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Message("hi".into()),
            "still idle -> peek untouched"
        );
    }

    #[test]
    fn dashboard_loaded_resets_peek_when_status_kind_changes() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::Idle,
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Message("hi".into());
        app.update(Msg::DashboardLoaded {
            sessions: vec![dash_session("s", 5, false, None)],
            permissions: vec![dash_perm("s")],
            questions: vec![],
        });
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Permission,
            "status flipped -> peek re-derived"
        );
    }

    #[test]
    fn derive_peek_idle_or_busy_row_is_loading() {
        let idle = DashboardState {
            rows: vec![DashboardRow {
                session: dash_session("s", 0, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            }],
            selected: 0,
            peek: DashboardPeek::Loading,
            ..DashboardState::default()
        };
        assert_eq!(idle.derive_peek(), DashboardPeek::Loading);

        let busy = DashboardState {
            rows: vec![DashboardRow {
                session: dash_session("s", 0, true, None),
                status: DashboardStatus::Busy,
                indent: false,
            }],
            selected: 0,
            peek: DashboardPeek::Loading,
            ..DashboardState::default()
        };
        assert_eq!(busy.derive_peek(), DashboardPeek::Loading);
    }

    #[test]
    fn dashboard_loaded_on_first_open_leaves_idle_row_peek_loading() {
        // Mirrors the dashboard opening for the first time (or a poll
        // finding a session for the first time): `App::new()` starts with
        // an empty `dashboard.rows`, so `prev` is `None` and the freshly
        // built row 0 is treated as a status change, re-deriving the peek.
        // An `Idle` row must land on `Loading` — the state
        // `maybe_fetch_dashboard_peek` (lib.rs) watches to fire the async
        // `GET /session/{id}/message` fetch that resolves it.
        let mut app = App::new();
        app.update(Msg::DashboardLoaded {
            sessions: vec![dash_session("s", 0, false, None)],
            permissions: vec![],
            questions: vec![],
        });
        assert_eq!(app.dashboard.selected, 0);
        assert_eq!(app.dashboard.rows[0].status, DashboardStatus::Idle);
        assert_eq!(app.dashboard.peek, DashboardPeek::Loading);
    }

    #[test]
    fn dashboard_peek_loaded_ignored_if_selection_moved() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a", 0, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 0, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        app.dashboard.selected = 1;
        app.update(Msg::DashboardPeekLoaded {
            session_id: "a".into(),
            text: "stale".into(),
        });
        assert_ne!(app.dashboard.peek, DashboardPeek::Message("stale".into()));
    }

    #[test]
    fn permission_reply_marks_matching_dashboard_row_idle() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::AwaitingPermission(dash_perm("s")),
            indent: false,
        }];
        app.update(Msg::PermissionReply {
            id: "perm_s".into(),
            reply: "once".into(),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
    }

    #[test]
    fn question_reply_marks_matching_dashboard_row_idle() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::AwaitingQuestion(dash_single_question("s")),
            indent: false,
        }];
        app.update(Msg::QuestionReply {
            id: "que_s".into(),
            reply: QuestionReplyKind::Answered(vec![vec![0]]),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
    }

    #[test]
    fn permission_reply_for_selected_row_resets_peek_to_loading() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::AwaitingPermission(dash_perm("s")),
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Permission;
        app.update(Msg::PermissionReply {
            id: "perm_s".into(),
            reply: "once".into(),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Loading,
            "replied-to selected row's stale Permission peek must be re-derived, \
             not left stale — Loading is what triggers the fetch"
        );
    }

    #[test]
    fn permission_reply_for_unselected_row_leaves_peek_untouched() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a", 0, false, None),
                status: DashboardStatus::AwaitingPermission(dash_perm("a")),
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 0, false, None),
                status: DashboardStatus::AwaitingPermission(dash_perm("b")),
                indent: false,
            },
        ];
        // Selected row is "b"; the reply is for "a".
        app.dashboard.selected = 1;
        app.dashboard.peek = DashboardPeek::Permission;
        app.update(Msg::PermissionReply {
            id: "perm_a".into(),
            reply: "once".into(),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
        assert!(matches!(
            app.dashboard.rows[1].status,
            DashboardStatus::AwaitingPermission(_)
        ));
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Permission,
            "reply to a non-selected row must not touch the selected row's peek"
        );
    }

    #[test]
    fn question_reply_for_selected_row_resets_peek_to_loading() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("s", 0, false, None),
            status: DashboardStatus::AwaitingQuestion(dash_single_question("s")),
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Question { highlight: 0 };
        app.update(Msg::QuestionReply {
            id: "que_s".into(),
            reply: QuestionReplyKind::Answered(vec![vec![0]]),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Loading,
            "replied-to selected row's stale Question peek must be re-derived"
        );
    }

    #[test]
    fn question_reply_for_unselected_row_leaves_peek_untouched() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a", 0, false, None),
                status: DashboardStatus::AwaitingQuestion(dash_single_question("a")),
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 0, false, None),
                status: DashboardStatus::AwaitingQuestion(dash_single_question("b")),
                indent: false,
            },
        ];
        app.dashboard.selected = 1;
        app.dashboard.peek = DashboardPeek::Question { highlight: 0 };
        app.update(Msg::QuestionReply {
            id: "que_a".into(),
            reply: QuestionReplyKind::Answered(vec![vec![0]]),
        });
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
        assert!(matches!(
            app.dashboard.rows[1].status,
            DashboardStatus::AwaitingQuestion(_)
        ));
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Question { highlight: 0 },
            "reply to a non-selected row must not touch the selected row's peek"
        );
    }

    #[test]
    fn open_dashboard_resets_state_and_opens_overlay() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("stale", 0, false, None),
            status: DashboardStatus::Idle,
            indent: false,
        }];
        app.open_dashboard();
        assert!(matches!(app.overlay, Overlay::Dashboard));
        assert!(app.dashboard.rows.is_empty(), "stale rows cleared on open");
    }

    #[test]
    fn open_dashboard_preserves_pinned_but_resets_filter_and_mode() {
        let mut app = App::new();
        app.dashboard.pinned.insert("fav".to_string());
        app.dashboard.filter = "stale filter".to_string();
        app.dashboard.mode = DashboardMode::Filter;
        app.open_dashboard();
        assert!(app.dashboard.pinned.contains("fav"), "pins survive reopen");
        assert_eq!(app.dashboard.filter, "", "filter resets on reopen");
        assert_eq!(
            app.dashboard.mode,
            DashboardMode::Browsing,
            "mode resets on reopen"
        );
    }

    #[test]
    fn session_busy_event_flips_matching_row_in_place() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a", 0, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 0, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        app.dashboard.selected = 1;
        app.dashboard.peek = DashboardPeek::Message("b's message".into());
        app.update(Msg::Event(ServerEvent::SessionBusy {
            session_id: "a".into(),
        }));
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Busy
        ));
        assert_eq!(
            app.dashboard.selected, 1,
            "flipping a non-selected row must not move selection"
        );
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Message("b's message".into()),
            "flipping a non-selected row must not touch the selected row's peek"
        );
    }

    #[test]
    fn session_idle_event_for_selected_row_rederives_peek() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("a", 0, true, None),
            status: DashboardStatus::Busy,
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Message("stale busy-row peek".into());
        app.update(Msg::Event(ServerEvent::SessionIdle {
            session_id: "a".into(),
        }));
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Loading,
            "flipping the selected row re-derives peek (Idle -> Loading, \
             which triggers the async re-fetch)"
        );
    }

    #[test]
    fn session_busy_event_does_not_clobber_pending_ask() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("a", 0, false, None),
            status: DashboardStatus::AwaitingPermission(dash_perm("a")),
            indent: false,
        }];
        app.dashboard.selected = 0;
        app.dashboard.peek = DashboardPeek::Permission;
        app.update(Msg::Event(ServerEvent::SessionBusy {
            session_id: "a".into(),
        }));
        assert!(
            matches!(
                app.dashboard.rows[0].status,
                DashboardStatus::AwaitingPermission(_)
            ),
            "a pending ask must take precedence over a bare busy push event"
        );
        assert_eq!(app.dashboard.peek, DashboardPeek::Permission);
    }

    #[test]
    fn session_busy_event_for_unknown_session_is_a_no_op() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("a", 0, false, None),
            status: DashboardStatus::Idle,
            indent: false,
        }];
        app.update(Msg::Event(ServerEvent::SessionBusy {
            session_id: "unrelated".into(),
        }));
        assert!(matches!(
            app.dashboard.rows[0].status,
            DashboardStatus::Idle
        ));
    }

    #[test]
    fn session_created_event_flags_refetch_only_when_dashboard_open() {
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.update(Msg::Event(ServerEvent::SessionCreated {
            session_id: "new".into(),
            title: None,
            parent_id: None,
        }));
        assert!(app.dashboard.needs_refetch);

        let mut app2 = App::new();
        // Dashboard not open.
        app2.update(Msg::Event(ServerEvent::SessionCreated {
            session_id: "new".into(),
            title: None,
            parent_id: None,
        }));
        assert!(!app2.dashboard.needs_refetch);
    }

    #[test]
    fn workflow_started_event_flags_refetch_only_when_dashboard_open() {
        let w = crate::sse::WorkflowMsg {
            phase: WfPhase::Started,
            session: "ses".into(),
            kind: "sdd".into(),
            arg: None,
            task_index: None,
            status: None,
            notes: String::new(),
            ok: None,
            summary: None,
            error: None,
        };
        let mut app = App::new();
        app.overlay = Overlay::Dashboard;
        app.update(Msg::Event(ServerEvent::Workflow(w.clone())));
        assert!(app.dashboard.needs_refetch);

        let mut app2 = App::new();
        app2.update(Msg::Event(ServerEvent::Workflow(w)));
        assert!(!app2.dashboard.needs_refetch);
    }

    #[test]
    fn dashboard_loaded_clears_needs_refetch() {
        let mut app = App::new();
        app.dashboard.needs_refetch = true;
        app.update(Msg::DashboardLoaded {
            sessions: vec![],
            permissions: vec![],
            questions: vec![],
        });
        assert!(!app.dashboard.needs_refetch);
    }

    #[test]
    fn dashboard_toggle_pin_moves_row_to_top_and_keeps_selection() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session("a", 30, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session("b", 20, false, None),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        app.dashboard.selected = 1; // "b"
        app.update(Msg::DashboardTogglePin);
        assert!(app.dashboard.pinned.contains("b"));
        let order: Vec<&str> = app
            .dashboard
            .rows
            .iter()
            .map(|r| r.session.id.as_str())
            .collect();
        assert_eq!(order, vec!["b", "a"], "pinned row floats to top");
        assert_eq!(
            app.dashboard.rows[app.dashboard.selected].session.id, "b",
            "selection follows the toggled row, not its old index"
        );

        // Toggling again unpins it.
        app.update(Msg::DashboardTogglePin);
        assert!(!app.dashboard.pinned.contains("b"));
    }

    #[test]
    fn dashboard_filter_changed_applies_live_and_preserves_selection() {
        let mut app = App::new();
        app.dashboard.rows = vec![
            DashboardRow {
                session: dash_session_titled("alpha", 20, None, "fix login bug"),
                status: DashboardStatus::Idle,
                indent: false,
            },
            DashboardRow {
                session: dash_session_titled("beta", 10, None, "unrelated"),
                status: DashboardStatus::Idle,
                indent: false,
            },
        ];
        app.dashboard.selected = 0; // "alpha"
        app.dashboard.peek = DashboardPeek::Message("alpha's message".into());
        app.update(Msg::DashboardFilterChanged("bug".into()));
        assert_eq!(app.dashboard.filter, "bug");
        assert_eq!(app.dashboard.rows.len(), 1);
        assert_eq!(app.dashboard.rows[0].session.id, "alpha");
        assert_eq!(
            app.dashboard.selected, 0,
            "the still-visible previously-selected row stays selected"
        );
        assert_eq!(
            app.dashboard.peek,
            DashboardPeek::Message("alpha's message".into()),
            "same row still selected -> peek must not flicker back to Loading"
        );
    }

    #[test]
    fn dashboard_session_created_inserts_row_and_selects_it() {
        let mut app = App::new();
        app.dashboard.rows = vec![DashboardRow {
            session: dash_session("existing", 10, false, None),
            status: DashboardStatus::Idle,
            indent: false,
        }];
        let new_session = dash_session("new", 20, false, None);
        app.update(Msg::DashboardSessionCreated(new_session));
        assert_eq!(app.dashboard.rows.len(), 2);
        assert_eq!(
            app.dashboard.rows[app.dashboard.selected].session.id, "new",
            "the newly created session is selected"
        );
    }

    #[test]
    fn create_dashboard_session_resets_mode_to_browsing() {
        let mut app = App::new();
        app.dashboard.mode = DashboardMode::NewSession("my title".into());
        app.update(Msg::CreateDashboardSession("my title".into()));
        assert_eq!(app.dashboard.mode, DashboardMode::Browsing);
    }

    #[test]
    fn palette_dashboard_command_opens_dashboard() {
        let mut app = App::new();
        app.open_palette();
        if let Overlay::Palette(ps) = &mut app.overlay {
            ps.query = "Dashboard".into();
        }
        let msg = app.palette_confirm();
        assert!(msg.is_none());
        assert!(matches!(app.overlay, Overlay::Dashboard));
    }
}
