//! The `question` tool — a port of the parameter surface of opencode
//! `packages/opencode/src/tool/question.ts`.
//!
//! Asking the user a question requires an interactive client (opencode threads
//! a `Question.Service`). That wiring lands with the UI layer, so
//! [`QuestionTool::execute`] decodes the faithful parameter shape (mirroring
//! `QuestionV1.Prompt`, `schema/src/v1/question.ts:14-30`) and returns a clear
//! error until a gate is available.

use serde::Deserialize;
use serde_json::Value;

use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError, decode_args};

/// One selectable option (`QuestionV1.Option`).
#[derive(Debug, Deserialize)]
struct QuestionOption {
    #[allow(dead_code)]
    label: String,
    #[allow(dead_code)]
    description: String,
}

/// One prompt (`QuestionV1.Prompt`, the base fields of `Info`).
#[derive(Debug, Deserialize)]
struct QuestionPrompt {
    #[allow(dead_code)]
    question: String,
    #[allow(dead_code)]
    header: String,
    #[allow(dead_code)]
    options: Vec<QuestionOption>,
    #[serde(default)]
    #[allow(dead_code)]
    multiple: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct QuestionParams {
    #[allow(dead_code)]
    questions: Vec<QuestionPrompt>,
}

/// The `question` tool (question.ts:14). Client-gated; stubbed until the UI
/// layer supplies an interactive gate.
#[derive(Debug, Default, Clone, Copy)]
pub struct QuestionTool;

#[async_trait::async_trait]
impl Tool for QuestionTool {
    fn id(&self) -> &str {
        "question"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/question.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "Complete question" },
                            "header": { "type": "string", "description": "Very short label (max 30 chars)" },
                            "options": {
                                "type": "array",
                                "description": "Available choices",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "Display text (1-5 words, concise)" },
                                        "description": { "type": "string", "description": "Explanation of choice" }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiple": { "type": "boolean", "description": "Allow selecting multiple choices" }
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let _params: QuestionParams = decode_args(self.id(), args)?;
        let _ = ctx;
        Err(ToolError::Execution(
            "question tool requires an interactive client".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requires_interactive_client() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = QuestionTool
            .execute(
                serde_json::json!({
                    "questions": [{
                        "question": "Pick one",
                        "header": "choice",
                        "options": [{ "label": "A", "description": "first" }]
                    }]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("interactive client"));
    }

    #[tokio::test]
    async fn bad_params_are_invalid_arguments() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = QuestionTool
            .execute(
                serde_json::json!({ "questions": [{ "header": "x" }] }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }
}
