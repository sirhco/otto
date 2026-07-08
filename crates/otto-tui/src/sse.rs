//! SSE frame decoding for the prompt stream (bare `LLMEvent`) and the `/event`
//! envelope stream (`{type, properties}`).

use otto_events::LLMEvent;
use serde::Deserialize;

/// Accumulates raw SSE bytes and yields each completed `data:` payload.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: String,
}

/// A decoded `/event` envelope frame — only the variants the TUI acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    Connected,
    PermissionAsked(PermissionAsked),
    Workflow(WorkflowMsg),
    Subagent(SubagentMsg),
    Other,
}

/// A decoded `workflow.subagent` envelope — one coalesced line of subagent tool
/// or text activity, session-guarded to the active run when folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentMsg {
    pub session: String,
    pub task_index: u32,
    pub verb: String,
    pub detail: String,
}

/// Which phase of a workflow run a [`ServerEvent::Workflow`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WfPhase {
    Started,
    Progress,
    Done,
}

/// A decoded `workflow.*` envelope from the `/event` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMsg {
    pub phase: WfPhase,
    /// The session id the workflow run is bound to (from the envelope). Needed
    /// so a cancel request targets the right run.
    pub session: String,
    pub kind: String,
    pub arg: Option<String>,
    pub task_index: Option<u32>,
    pub status: Option<String>,
    pub notes: String,
    pub ok: Option<bool>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// A pending permission surfaced by `/event` (`permission.asked`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAsked {
    pub id: String,
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a chunk of raw SSE bytes; return each completed frame's `data:`
    /// payload. Frames are `\n\n`-separated; non-`data:` lines (comments,
    /// event:, id:) are dropped. A `data:` payload spanning multiple lines is
    /// joined with `\n` (SSE spec), though otto emits single-line data.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let raw = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            let data: Vec<&str> = raw
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("data:")
                        .map(|d| d.strip_prefix(' ').unwrap_or(d))
                })
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

/// Parse a bare prompt-stream frame into an [`LLMEvent`]. Returns `None` for
/// frames that are not valid events (never panics on malformed input).
#[must_use]
pub fn decode_llm(frame: &str) -> Option<LLMEvent> {
    serde_json::from_str(frame).ok()
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: serde_json::Value,
}

#[derive(Deserialize)]
struct PermProps {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    permission: String,
    #[serde(default)]
    patterns: Vec<String>,
}

/// Parse a `/event` envelope frame. Unknown envelope types map to
/// [`ServerEvent::Other`]; malformed frames also map to `Other`.
#[must_use]
pub fn decode_event(frame: &str) -> ServerEvent {
    let Ok(env) = serde_json::from_str::<Envelope>(frame) else {
        return ServerEvent::Other;
    };
    match env.kind.as_str() {
        "server.connected" => ServerEvent::Connected,
        "permission.asked" => match serde_json::from_value::<PermProps>(env.properties) {
            Ok(p) => ServerEvent::PermissionAsked(PermissionAsked {
                id: p.id,
                session_id: p.session_id,
                permission: p.permission,
                patterns: p.patterns,
            }),
            Err(_) => ServerEvent::Other,
        },
        "workflow.started" | "workflow.progress" | "workflow.done" => {
            let p = &env.properties;
            let phase = match env.kind.as_str() {
                "workflow.started" => WfPhase::Started,
                "workflow.progress" => WfPhase::Progress,
                _ => WfPhase::Done,
            };
            ServerEvent::Workflow(WorkflowMsg {
                phase,
                session: p
                    .get("session")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                kind: p
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                arg: p.get("arg").and_then(|v| v.as_str()).map(str::to_string),
                task_index: p
                    .get("task_index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| n as u32),
                status: p.get("status").and_then(|v| v.as_str()).map(str::to_string),
                notes: p
                    .get("notes")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                ok: p.get("ok").and_then(serde_json::Value::as_bool),
                summary: p
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                error: p.get("error").and_then(|v| v.as_str()).map(str::to_string),
            })
        }
        "workflow.subagent" => {
            let p = &env.properties;
            ServerEvent::Subagent(SubagentMsg {
                session: p
                    .get("session")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                task_index: p
                    .get("task_index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                verb: p
                    .get("verb")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                detail: p
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        }
        _ => ServerEvent::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frames_and_strips_data_prefix() {
        let mut d = FrameDecoder::new();
        let out = d.push(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }

    #[test]
    fn buffers_partial_frames_across_pushes() {
        let mut d = FrameDecoder::new();
        assert!(d.push(b"data: {\"x\":").is_empty());
        let out = d.push(b"42}\n\n");
        assert_eq!(out, vec!["{\"x\":42}".to_string()]);
    }

    #[test]
    fn ignores_comment_keepalive_lines() {
        let mut d = FrameDecoder::new();
        let out = d.push(b": keep-alive\n\ndata: {\"ok\":true}\n\n");
        assert_eq!(out, vec!["{\"ok\":true}".to_string()]);
    }

    #[test]
    fn decode_llm_reads_text_delta() {
        let ev = decode_llm("{\"type\":\"text-delta\",\"id\":\"t1\",\"text\":\"hi\"}").unwrap();
        assert!(matches!(ev, LLMEvent::TextDelta { text, .. } if text == "hi"));
    }

    #[test]
    fn decode_event_reads_permission_asked() {
        let frame = "{\"type\":\"permission.asked\",\"properties\":{\"id\":\"perm_1\",\"sessionID\":\"ses_1\",\"permission\":\"edit\",\"patterns\":[\"*.rs\"],\"metadata\":{}}}";
        let ev = decode_event(frame);
        assert_eq!(
            ev,
            ServerEvent::PermissionAsked(PermissionAsked {
                id: "perm_1".into(),
                session_id: "ses_1".into(),
                permission: "edit".into(),
                patterns: vec!["*.rs".into()],
            })
        );
    }

    #[test]
    fn decodes_workflow_started() {
        let frame = "{\"type\":\"workflow.started\",\"properties\":{\"session\":\"s\",\"kind\":\"sdd\",\"arg\":\"p.md\"}}";
        match decode_event(frame) {
            ServerEvent::Workflow(w) => {
                assert_eq!(w.phase, WfPhase::Started);
                assert_eq!(w.kind, "sdd");
                assert_eq!(w.arg.as_deref(), Some("p.md"));
            }
            other => panic!("expected Workflow, got {other:?}"),
        }
    }

    #[test]
    fn decodes_workflow_progress_and_done() {
        let p = "{\"type\":\"workflow.progress\",\"properties\":{\"session\":\"s\",\"kind\":\"sdd\",\"task_index\":2,\"status\":\"DONE\",\"notes\":\"review clean\"}}";
        match decode_event(p) {
            ServerEvent::Workflow(w) => {
                assert_eq!(w.phase, WfPhase::Progress);
                assert_eq!(w.task_index, Some(2));
                assert_eq!(w.status.as_deref(), Some("DONE"));
                assert_eq!(w.notes, "review clean");
            }
            o => panic!("{o:?}"),
        }
        let d = "{\"type\":\"workflow.done\",\"properties\":{\"session\":\"s\",\"kind\":\"sdd\",\"ok\":true,\"summary\":\"3 task(s) processed\",\"error\":null}}";
        match decode_event(d) {
            ServerEvent::Workflow(w) => {
                assert_eq!(w.phase, WfPhase::Done);
                assert_eq!(w.ok, Some(true));
                assert_eq!(w.summary.as_deref(), Some("3 task(s) processed"));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn decodes_workflow_subagent() {
        let f = "{\"type\":\"workflow.subagent\",\"properties\":{\"session\":\"s\",\"task_index\":2,\"verb\":\"bash\",\"detail\":\"cargo test\"}}";
        match decode_event(f) {
            ServerEvent::Subagent(m) => {
                assert_eq!(m.session, "s");
                assert_eq!(m.task_index, 2);
                assert_eq!(m.verb, "bash");
                assert_eq!(m.detail, "cargo test");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn decode_event_reads_connected_and_other() {
        assert_eq!(
            decode_event("{\"type\":\"server.connected\",\"properties\":{}}"),
            ServerEvent::Connected
        );
        assert_eq!(
            decode_event("{\"type\":\"message.part.updated\",\"properties\":{}}"),
            ServerEvent::Other
        );
    }
}
