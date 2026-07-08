//! Tests for the `otto-events` public API.
//!
//! Verifies serde round-trips and exact `type` tag strings against the shapes
//! opencode `packages/llm/src/schema/events.ts` emits, plus the `Usage`
//! invariant and the `EventBus` pub/sub behaviour.

use otto_events::{
    EventBus, FinishReason, LLMEvent, ProviderFailureClassification, ToolOutput, ToolResultValue,
    Usage,
};
use serde_json::json;

/// Serialize an event, assert its `type` tag matches `expected_tag`, then
/// deserialize it back and assert value equality.
fn round_trip(event: &LLMEvent, expected_tag: &str) {
    let value = serde_json::to_value(event).expect("serialize");
    assert_eq!(
        value.get("type").and_then(|t| t.as_str()),
        Some(expected_tag),
        "type tag mismatch for {event:?}",
    );
    let back: LLMEvent = serde_json::from_value(value).expect("deserialize");
    assert_eq!(&back, event, "round trip mismatch for {expected_tag}");
}

#[test]
fn round_trips_every_variant_with_exact_tags() {
    let meta = json!({ "anthropic": { "raw": 1 } });

    let cases: Vec<(LLMEvent, &str)> = vec![
        (LLMEvent::StepStart { index: 0 }, "step-start"),
        (
            LLMEvent::TextStart {
                id: "cb_1".into(),
                provider_metadata: Some(meta.clone()),
            },
            "text-start",
        ),
        (
            LLMEvent::TextDelta {
                id: "cb_1".into(),
                text: "hello".into(),
                provider_metadata: None,
            },
            "text-delta",
        ),
        (
            LLMEvent::TextEnd {
                id: "cb_1".into(),
                provider_metadata: None,
            },
            "text-end",
        ),
        (
            LLMEvent::ReasoningStart {
                id: "rb_1".into(),
                provider_metadata: None,
            },
            "reasoning-start",
        ),
        (
            LLMEvent::ReasoningDelta {
                id: "rb_1".into(),
                text: "thinking".into(),
                provider_metadata: None,
            },
            "reasoning-delta",
        ),
        (
            LLMEvent::ReasoningEnd {
                id: "rb_1".into(),
                provider_metadata: None,
            },
            "reasoning-end",
        ),
        (
            LLMEvent::ToolInputStart {
                id: "tc_1".into(),
                name: "search".into(),
                provider_metadata: None,
            },
            "tool-input-start",
        ),
        (
            LLMEvent::ToolInputDelta {
                id: "tc_1".into(),
                name: "search".into(),
                text: "{\"q\":".into(),
            },
            "tool-input-delta",
        ),
        (
            LLMEvent::ToolInputEnd {
                id: "tc_1".into(),
                name: "search".into(),
                provider_metadata: None,
            },
            "tool-input-end",
        ),
        (
            LLMEvent::ToolCall {
                id: "tc_1".into(),
                name: "search".into(),
                input: json!({ "q": "rust" }),
                provider_executed: Some(true),
                provider_metadata: Some(meta.clone()),
            },
            "tool-call",
        ),
        (
            LLMEvent::ToolResult {
                id: "tc_1".into(),
                name: "search".into(),
                result: ToolResultValue::Json {
                    value: json!({ "hits": 3 }),
                },
                output: Some(ToolOutput {
                    structured: json!({ "hits": 3 }),
                    content: vec![json!({ "type": "text", "text": "ok" })],
                }),
                provider_executed: Some(false),
                provider_metadata: None,
            },
            "tool-result",
        ),
        (
            LLMEvent::ToolError {
                id: "tc_1".into(),
                name: "search".into(),
                message: "boom".into(),
                error: Some(json!({ "code": 500 })),
                provider_metadata: None,
            },
            "tool-error",
        ),
        (
            LLMEvent::StepFinish {
                index: 0,
                reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    ..Usage::default()
                }),
                provider_metadata: None,
            },
            "step-finish",
        ),
        (
            LLMEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            },
            "finish",
        ),
        (
            LLMEvent::ProviderError {
                message: "context too long".into(),
                classification: Some(ProviderFailureClassification::ContextOverflow),
                retryable: Some(false),
                provider_metadata: None,
            },
            "provider-error",
        ),
    ];

    // Guard: exactly the 16 variants of the union are exercised.
    assert_eq!(cases.len(), 16);

    for (event, tag) in &cases {
        round_trip(event, tag);
    }
}

#[test]
fn deserializes_literal_opencode_json() {
    // tool-input-delta — guards against field-name drift (no providerMetadata).
    let value = json!({
        "type": "tool-input-delta",
        "id": "call_abc",
        "name": "get_weather",
        "text": "{\"city\":\"NYC\"}"
    });
    let event: LLMEvent = serde_json::from_value(value).unwrap();
    assert_eq!(
        event,
        LLMEvent::ToolInputDelta {
            id: "call_abc".into(),
            name: "get_weather".into(),
            text: "{\"city\":\"NYC\"}".into(),
        }
    );

    // step-finish with camelCase providerMetadata + nested Usage camelCase.
    let value = json!({
        "type": "step-finish",
        "index": 2,
        "reason": "content-filter",
        "usage": {
            "inputTokens": 100,
            "outputTokens": 40,
            "nonCachedInputTokens": 70,
            "cacheReadInputTokens": 20,
            "cacheWriteInputTokens": 10,
            "reasoningTokens": 8
        },
        "providerMetadata": { "openai": { "system_fingerprint": "fp_1" } }
    });
    let event: LLMEvent = serde_json::from_value(value).unwrap();
    let LLMEvent::StepFinish {
        index,
        reason,
        usage,
        provider_metadata,
    } = event
    else {
        panic!("expected step-finish");
    };
    assert_eq!(index, 2);
    assert_eq!(reason, FinishReason::ContentFilter);
    assert!(provider_metadata.is_some());
    let usage = usage.expect("usage present");
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.non_cached_input_tokens, Some(70));
    assert_eq!(usage.reasoning_tokens, Some(8));
    assert!(usage.invariant_holds());

    // provider-error with classification literal.
    let value = json!({
        "type": "provider-error",
        "message": "too long",
        "classification": "context-overflow",
        "retryable": false
    });
    let event: LLMEvent = serde_json::from_value(value).unwrap();
    assert_eq!(
        event,
        LLMEvent::ProviderError {
            message: "too long".into(),
            classification: Some(ProviderFailureClassification::ContextOverflow),
            retryable: Some(false),
            provider_metadata: None,
        }
    );
}

#[test]
fn usage_invariant_holds_for_consistent_breakdown() {
    // Anthropic-style: breakdown sums to inclusive input; reasoning subset of output.
    let usage = Usage {
        input_tokens: Some(100),
        output_tokens: Some(40),
        non_cached_input_tokens: Some(70),
        cache_read_input_tokens: Some(20),
        cache_write_input_tokens: Some(10),
        reasoning_tokens: Some(8),
        total_tokens: Some(140),
        provider_metadata: None,
    };
    assert!(usage.invariant_holds());
    assert_eq!(usage.visible_output_tokens(), 32);
}

#[test]
fn usage_invariant_detects_violations() {
    // Breakdown does not sum to inputTokens (70 + 20 + 10 != 90).
    let bad_input = Usage {
        input_tokens: Some(90),
        non_cached_input_tokens: Some(70),
        cache_read_input_tokens: Some(20),
        cache_write_input_tokens: Some(10),
        ..Usage::default()
    };
    assert!(!bad_input.invariant_holds());

    // reasoning exceeds output.
    let bad_reasoning = Usage {
        output_tokens: Some(5),
        reasoning_tokens: Some(9),
        ..Usage::default()
    };
    assert!(!bad_reasoning.invariant_holds());
    assert_eq!(bad_reasoning.visible_output_tokens(), 0);
}

#[tokio::test]
async fn event_bus_delivers_to_multiple_subscribers() {
    let bus: EventBus<LLMEvent> = EventBus::new();

    let mut sub_a = bus.subscribe();
    let mut sub_b = bus.subscribe();
    assert_eq!(bus.subscriber_count(), 2);

    let event = LLMEvent::TextDelta {
        id: "cb_1".into(),
        text: "hi".into(),
        provider_metadata: None,
    };
    let delivered = bus.publish(event.clone());
    assert_eq!(delivered, 2);

    let got_a = sub_a.recv().await.expect("sub a receives");
    let got_b = sub_b.recv().await.expect("sub b receives");
    assert_eq!(*got_a, event);
    assert_eq!(*got_b, event);
}

#[tokio::test]
async fn event_bus_publish_without_subscribers_is_zero() {
    let bus: EventBus<LLMEvent> = EventBus::new();
    let delivered = bus.publish(LLMEvent::StepStart { index: 0 });
    assert_eq!(delivered, 0);
}
