//! The OpenAI-compatible Chat protocol.
//!
//! Faithful port of opencode
//! `packages/llm/src/protocols/openai-compatible-chat.ts`. Non-OpenAI providers
//! (DeepSeek, TogetherAI, Cerebras, Fireworks, …) expose an OpenAI Chat
//! `/chat/completions` endpoint whose wire body and streaming reducer are
//! identical to native OpenAI. This wrapper reuses [`OpenAIChat`]'s protocol
//! logic verbatim and overrides only the route id so providers resolve
//! per-family without colliding with native OpenAI. The per-provider `baseURL`
//! is configured by the provider in the later providers task.

use otto_events::LLMEvent;

use crate::error::LLMError;
use crate::protocol::Protocol;
use crate::protocols::openai_chat::{OpenAIChat, OpenAIChatBody, OpenAIChatEvent, ParserState};
use crate::request::LLMRequest;

/// Protocol id (`ADAPTER` in `openai-compatible-chat.ts:6`).
const ADAPTER: &str = "openai-compatible-chat";

/// The OpenAI-compatible Chat protocol — delegates every operation to
/// [`OpenAIChat`], overriding only [`Protocol::id`].
pub struct OpenAICompatibleChat;

impl Protocol for OpenAICompatibleChat {
    type Body = OpenAIChatBody;
    type Event = OpenAIChatEvent;
    type State = ParserState;

    fn id(&self) -> &'static str {
        ADAPTER
    }

    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        OpenAIChat.build_body(req)
    }

    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        OpenAIChat.decode_event(frame)
    }

    fn initial(&self, req: &LLMRequest) -> Self::State {
        OpenAIChat.initial(req)
    }

    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        OpenAIChat.step(state, event)
    }

    fn on_halt(&self, state: &mut Self::State) -> Vec<LLMEvent> {
        OpenAIChat.on_halt(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_openai_compatible_chat() {
        assert_eq!(OpenAICompatibleChat.id(), "openai-compatible-chat");
    }
}
