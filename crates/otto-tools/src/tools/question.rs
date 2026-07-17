//! The `question` tool — a port of the parameter surface of opencode
//! `packages/opencode/src/tool/question.ts`.

use serde::Deserialize;
use serde_json::Value;

use crate::tool::{
    ExecuteResult, QuestionOutcome, QuestionPrompt, Tool, ToolContext, ToolError, decode_args,
};

#[derive(Debug, Deserialize)]
struct QuestionParams {
    questions: Vec<QuestionPrompt>,
}

/// The `question` tool (question.ts:14).
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
        let params: QuestionParams = decode_args(self.id(), args)?;
        let questions = params.questions;

        match ctx.question.ask(questions.clone()).await {
            QuestionOutcome::Cancelled => {
                Err(ToolError::Execution("question cancelled by user".to_string()))
            }
            QuestionOutcome::Answered(answers) => {
                if answers.len() != questions.len() {
                    return Err(ToolError::Execution(format!(
                        "answer count {} does not match question count {}",
                        answers.len(),
                        questions.len()
                    )));
                }
                for (i, (sel, q)) in answers.iter().zip(questions.iter()).enumerate() {
                    if sel.is_empty() || sel.iter().any(|&idx| idx >= q.options.len()) {
                        return Err(ToolError::Execution(format!(
                            "question {i}: invalid selection"
                        )));
                    }
                    if !q.multiple && sel.len() != 1 {
                        return Err(ToolError::Execution(format!(
                            "question {i}: expected exactly one selection"
                        )));
                    }
                }
                let output = questions
                    .iter()
                    .zip(answers.iter())
                    .map(|(q, sel)| {
                        let labels: Vec<&str> =
                            sel.iter().map(|&i| q.options[i].label.as_str()).collect();
                        format!("{}: {}", q.header, labels.join(", "))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ExecuteResult::new("question", output))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{QuestionGate, QuestionOutcome};

    struct ScriptedGate(QuestionOutcome);

    #[async_trait::async_trait]
    impl QuestionGate for ScriptedGate {
        async fn ask(&self, _questions: Vec<QuestionPrompt>) -> QuestionOutcome {
            self.0.clone()
        }
    }

    fn ctx_with_gate(outcome: QuestionOutcome) -> ToolContext {
        ToolContext::builder(std::env::temp_dir())
            .question(std::sync::Arc::new(ScriptedGate(outcome)))
            .build()
    }

    #[tokio::test]
    async fn default_gate_cancels_and_errors() {
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
        assert!(err.to_string().contains("cancelled"));
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

    #[tokio::test]
    async fn single_question_single_select_answered() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![0]]));
        let out = QuestionTool
            .execute(
                serde_json::json!({
                    "questions": [{
                        "question": "Pick one",
                        "header": "choice",
                        "options": [
                            { "label": "A", "description": "first" },
                            { "label": "B", "description": "second" }
                        ]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("answered");
        assert_eq!(out.output, "choice: A");
    }

    #[tokio::test]
    async fn multi_question_batch_answered() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![1], vec![0]]));
        let out = QuestionTool
            .execute(
                serde_json::json!({
                    "questions": [
                        {
                            "question": "Pick one",
                            "header": "first",
                            "options": [
                                { "label": "A", "description": "a" },
                                { "label": "B", "description": "b" }
                            ]
                        },
                        {
                            "question": "Pick another",
                            "header": "second",
                            "options": [
                                { "label": "X", "description": "x" },
                                { "label": "Y", "description": "y" }
                            ]
                        }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("answered");
        assert_eq!(out.output, "first: B\nsecond: X");
    }

    #[tokio::test]
    async fn multi_select_answered() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![0, 2]]));
        let out = QuestionTool
            .execute(
                serde_json::json!({
                    "questions": [{
                        "question": "Pick some",
                        "header": "multi",
                        "multiple": true,
                        "options": [
                            { "label": "A", "description": "a" },
                            { "label": "B", "description": "b" },
                            { "label": "C", "description": "c" }
                        ]
                    }]
                }),
                &ctx,
            )
            .await
            .expect("answered");
        assert_eq!(out.output, "multi: A, C");
    }

    #[tokio::test]
    async fn cancelled_is_an_error() {
        let ctx = ctx_with_gate(QuestionOutcome::Cancelled);
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
        assert!(err.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn answer_count_mismatch_is_an_error() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![0], vec![0]]));
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
        assert!(err.to_string().contains("answer count"));
    }

    #[tokio::test]
    async fn out_of_range_index_is_an_error() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![5]]));
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
        assert!(err.to_string().contains("invalid selection"));
    }

    #[tokio::test]
    async fn multiple_selections_on_non_multiple_question_is_an_error() {
        let ctx = ctx_with_gate(QuestionOutcome::Answered(vec![vec![0, 1]]));
        let err = QuestionTool
            .execute(
                serde_json::json!({
                    "questions": [{
                        "question": "Pick one",
                        "header": "choice",
                        "options": [
                            { "label": "A", "description": "first" },
                            { "label": "B", "description": "second" }
                        ]
                    }]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expected exactly one"));
    }
}
