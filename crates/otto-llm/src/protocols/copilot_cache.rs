//! GitHub Copilot's `copilot_cache_control` prompt-caching wrapper.
//!
//! Copilot rides on top of an existing wire protocol (OpenAI Chat or
//! Anthropic Messages) but expects extra `copilot_cache_control` markers on
//! specific messages/content-parts, and rejects `max_tokens` for `gpt*`
//! models. [`CopilotCache`] wraps any base [`Protocol`], re-emitting its
//! request body as a mutated `serde_json::Value` while delegating every other
//! method straight through to the inner protocol.
//!
//! Port of opencode's Copilot `transform.ts` cache-control injection
//! (`transform.ts:324-368`).

use serde_json::Value;

use crate::error::LLMError;
use crate::protocol::Protocol;
use crate::request::LLMRequest;
use otto_events::LLMEvent;

/// Which wire shape the inner protocol produces, and therefore which
/// cache-control injection rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyShape {
    OpenAi,
    Anthropic,
}

/// Wraps a base protocol, re-emitting its request body as JSON with Copilot's
/// `copilot_cache_control` markers (and, for gpt models, `max_tokens`
/// removed). Decode/step/state pass straight through to the inner protocol.
pub struct CopilotCache<P> {
    inner: P,
    shape: BodyShape,
    model_id: String,
}

impl<P> CopilotCache<P> {
    #[must_use]
    pub fn new(inner: P, shape: BodyShape, model_id: impl Into<String>) -> Self {
        Self {
            inner,
            shape,
            model_id: model_id.into(),
        }
    }
}

impl<P: Protocol> Protocol for CopilotCache<P> {
    type Body = Value;
    type Event = P::Event;
    type State = P::State;

    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn build_body(&self, req: &LLMRequest) -> Result<Value, LLMError> {
        let inner = self.inner.build_body(req)?;
        let mut v = serde_json::to_value(inner).map_err(|e| LLMError::Body(e.to_string()))?;
        if self.model_id.starts_with("gpt") {
            strip_max_tokens(&mut v);
        }
        match self.shape {
            BodyShape::OpenAi => apply_openai_cache(&mut v),
            BodyShape::Anthropic => apply_anthropic_cache(&mut v),
        }
        Ok(v)
    }

    fn decode_event(&self, frame: &str) -> Result<P::Event, LLMError> {
        self.inner.decode_event(frame)
    }

    fn initial(&self, req: &LLMRequest) -> P::State {
        self.inner.initial(req)
    }

    fn step(&self, state: &mut P::State, event: P::Event) -> Result<Vec<LLMEvent>, LLMError> {
        self.inner.step(state, event)
    }

    fn terminal(&self, event: &P::Event) -> bool {
        self.inner.terminal(event)
    }

    fn on_halt(&self, state: &mut P::State) -> Vec<LLMEvent> {
        self.inner.on_halt(state)
    }
}

fn cache_marker() -> Value {
    serde_json::json!({ "type": "ephemeral" })
}

/// Attach `copilot_cache_control` to a message: on the last content part when
/// `content` is an array, else on the message object (copilot is content-level
/// when content is structured — transform.ts:349-368).
fn mark_cache(msg: &mut Value) {
    if let Some(last) = msg
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
        .and_then(|parts| parts.last_mut())
    {
        if let Some(o) = last.as_object_mut() {
            o.insert("copilot_cache_control".to_string(), cache_marker());
        }
        return;
    }
    if let Some(o) = msg.as_object_mut() {
        o.insert("copilot_cache_control".to_string(), cache_marker());
    }
}

/// OpenAI-chat body: mark the first 2 `role:"system"` and last 2 non-system
/// messages (transform.ts:324-348).
fn apply_openai_cache(v: &mut Value) {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    let is_sys = |m: &Value| m.get("role").and_then(|r| r.as_str()) == Some("system");
    let sys: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| is_sys(m))
        .map(|(i, _)| i)
        .take(2)
        .collect();
    let non_sys: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| !is_sys(m))
        .map(|(i, _)| i)
        .collect();
    let tail = &non_sys[non_sys.len().saturating_sub(2)..];
    for &i in sys.iter().chain(tail) {
        mark_cache(&mut msgs[i]);
    }
}

/// Anthropic body: mark the first 2 `system` blocks (when `system` is an array)
/// and the last 2 `messages`.
fn apply_anthropic_cache(v: &mut Value) {
    if let Some(sys) = v.get_mut("system").and_then(|s| s.as_array_mut()) {
        for b in sys.iter_mut().take(2) {
            if let Some(o) = b.as_object_mut() {
                o.insert("copilot_cache_control".to_string(), cache_marker());
            }
        }
    }
    if let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let start = msgs.len().saturating_sub(2);
        for m in msgs.iter_mut().skip(start) {
            mark_cache(m);
        }
    }
}

/// Copilot rejects `max_tokens` for `gpt*` models (opencode forces it undefined).
fn strip_max_tokens(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.remove("max_tokens");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_cache_marks_first2_system_and_last2_nonsystem() {
        let mut v = json!({"messages": [
            {"role":"system","content":"s0"},
            {"role":"system","content":"s1"},
            {"role":"system","content":"s2"},
            {"role":"user","content":"u0"},
            {"role":"assistant","content":"a0"},
            {"role":"user","content":"u1"},
        ]});
        apply_openai_cache(&mut v);
        let m = v["messages"].as_array().unwrap();
        let has = |i: usize| m[i].get("copilot_cache_control").is_some();
        // first 2 system (0,1) yes; 3rd system (2) no
        assert!(has(0) && has(1) && !has(2));
        // last 2 non-system (a0=4, u1=5) yes; first non-system (u0=3) no
        assert!(!has(3) && has(4) && has(5));
        assert_eq!(m[0]["copilot_cache_control"], json!({"type":"ephemeral"}));
    }

    #[test]
    fn openai_cache_marks_last_content_part_when_array() {
        let mut v = json!({"messages": [
            {"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
        ]});
        apply_openai_cache(&mut v);
        let parts = v["messages"][0]["content"].as_array().unwrap();
        assert!(parts[0].get("copilot_cache_control").is_none());
        assert!(parts[1].get("copilot_cache_control").is_some());
        assert!(v["messages"][0].get("copilot_cache_control").is_none());
    }

    #[test]
    fn anthropic_cache_marks_first2_system_blocks_and_last2_messages() {
        let mut v = json!({
            "system": [{"type":"text","text":"s0"},{"type":"text","text":"s1"},{"type":"text","text":"s2"}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"u0"}]},
                {"role":"assistant","content":[{"type":"text","text":"a0"}]},
                {"role":"user","content":[{"type":"text","text":"u1"}]},
            ]
        });
        apply_anthropic_cache(&mut v);
        let sys = v["system"].as_array().unwrap();
        assert!(
            sys[0].get("copilot_cache_control").is_some()
                && sys[1].get("copilot_cache_control").is_some()
                && sys[2].get("copilot_cache_control").is_none()
        );
        let m = v["messages"].as_array().unwrap();
        // last 2 messages, on their last content part
        assert!(m[0]["content"][0].get("copilot_cache_control").is_none());
        assert!(m[1]["content"][0].get("copilot_cache_control").is_some());
        assert!(m[2]["content"][0].get("copilot_cache_control").is_some());
    }

    #[test]
    fn strip_max_tokens_removes_key() {
        let mut v = json!({"model":"gpt-4o","max_tokens":1000,"messages":[]});
        strip_max_tokens(&mut v);
        assert!(v.get("max_tokens").is_none());
    }

    #[test]
    fn wrapper_delegates_id_to_inner() {
        use crate::protocols::openai_chat::OpenAIChat;

        let wrapped = CopilotCache::new(OpenAIChat, BodyShape::OpenAi, "gpt-4o");
        assert_eq!(wrapped.id(), OpenAIChat.id());
    }
}
