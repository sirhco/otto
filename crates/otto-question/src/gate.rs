//! The [`otto_tools::tool::QuestionGate`] adapter that the session loop
//! injects into `ToolContext`.

use std::sync::Arc;

use async_trait::async_trait;
use otto_tools::tool::{QuestionGate, QuestionOutcome, QuestionPrompt};

use crate::question::{Question, SessionId};

/// A [`QuestionGate`] bound to one session that delegates to a shared
/// [`Question`] service.
#[derive(Clone)]
pub struct SessionQuestionGate {
    question: Arc<Question>,
    session_id: SessionId,
}

impl SessionQuestionGate {
    /// Bind `question` to `session_id`.
    #[must_use]
    pub fn new(question: Arc<Question>, session_id: impl Into<SessionId>) -> Self {
        Self {
            question,
            session_id: session_id.into(),
        }
    }
}

#[async_trait]
impl QuestionGate for SessionQuestionGate {
    async fn ask(&self, questions: Vec<QuestionPrompt>) -> QuestionOutcome {
        self.question.ask(self.session_id.clone(), questions).await
    }
}
