//! The `websearch` tool — a port of opencode
//! `packages/opencode/src/tool/websearch.ts`.
//!
//! opencode gates the search behind a provider (exa / parallel) selected from
//! runtime flags and env (`websearch.ts:30-37`). Here the provider is an
//! injectable [`WebSearchProvider`] seam; when none is configured the tool
//! returns a clear [`ToolError::Execution`] rather than hardcoding any API key.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

/// Parameters for `websearch` (websearch.ts:10-25).
#[derive(Debug, Clone, Deserialize)]
pub struct WebSearchQuery {
    /// The search query.
    pub query: String,
    /// Number of results to return (default 8).
    #[serde(rename = "numResults", default)]
    pub num_results: Option<u32>,
    /// Live-crawl mode: `fallback` or `preferred`.
    #[serde(default)]
    pub livecrawl: Option<String>,
    /// Search type: `auto`, `fast`, or `deep`.
    #[serde(rename = "type", default)]
    pub search_type: Option<String>,
    /// Max characters of LLM-optimized context.
    #[serde(rename = "contextMaxCharacters", default)]
    pub context_max_characters: Option<u32>,
}

/// The provider seam. A concrete provider (exa/parallel/opencode) implements
/// this; the session layer injects it. Absent a provider the tool errors.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// A human label for the provider (e.g. `Exa Web Search`).
    fn label(&self) -> &str;
    /// Run a search, returning LLM-facing text.
    async fn search(&self, query: &WebSearchQuery, ctx: &ToolContext) -> Result<String, ToolError>;
}

/// The `websearch` tool (websearch.ts:99).
#[derive(Default, Clone)]
pub struct WebSearchTool {
    provider: Option<Arc<dyn WebSearchProvider>>,
}

impl WebSearchTool {
    /// Build the tool with an explicit provider.
    pub fn with_provider(provider: Arc<dyn WebSearchProvider>) -> Self {
        Self {
            provider: Some(provider),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn id(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/websearch.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Websearch query" },
                "numResults": { "type": "number", "description": "Number of search results to return (default: 8)" },
                "livecrawl": {
                    "type": "string",
                    "enum": ["fallback", "preferred"],
                    "description": "Live crawl mode (default: 'fallback')"
                },
                "type": {
                    "type": "string",
                    "enum": ["auto", "fast", "deep"],
                    "description": "Search type (default: 'auto')"
                },
                "contextMaxCharacters": {
                    "type": "number",
                    "description": "Maximum characters for context string optimized for LLMs (default: 10000)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: WebSearchQuery = decode_args(self.id(), args)?;

        let Some(provider) = &self.provider else {
            return Err(ToolError::Execution(
                "no web search provider is configured (set OTTO_WEBSEARCH_PROVIDER and the corresponding API key)".to_string(),
            ));
        };

        let label = provider.label().to_string();
        ctx.metadata.update(
            Some(format!("{label} \"{}\"", params.query)),
            Some(serde_json::json!({ "provider": label })),
        );

        ctx.permission
            .ask(PermissionRequest {
                permission: "websearch".to_string(),
                patterns: vec![params.query.clone()],
                always: vec!["*".to_string()],
                metadata: serde_json::json!({ "query": params.query, "provider": label }),
            })
            .await?;

        let result = provider.search(&params, ctx).await?;
        let output = if result.is_empty() {
            "No search results found. Please try a different query.".to_string()
        } else {
            result
        };

        Ok(
            ExecuteResult::new(format!("{label}: {}", params.query), output)
                .with_metadata(serde_json::json!({ "provider": label })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unconfigured_provider_errors_clearly() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = WebSearchTool::default()
            .execute(serde_json::json!({ "query": "rust async" }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no web search provider is configured")
        );
    }

    #[tokio::test]
    async fn params_decode_full_shape() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        // A missing required query is InvalidArguments.
        let err = WebSearchTool::default()
            .execute(serde_json::json!({ "numResults": 5 }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }

    #[tokio::test]
    async fn injected_provider_runs() {
        struct Fake;
        #[async_trait]
        impl WebSearchProvider for Fake {
            fn label(&self) -> &str {
                "Fake Search"
            }
            async fn search(
                &self,
                query: &WebSearchQuery,
                _ctx: &ToolContext,
            ) -> Result<String, ToolError> {
                Ok(format!("results for {}", query.query))
            }
        }
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let tool = WebSearchTool::with_provider(Arc::new(Fake));
        let res = tool
            .execute(serde_json::json!({ "query": "hello" }), &ctx)
            .await
            .unwrap();
        assert_eq!(res.output, "results for hello");
        assert_eq!(res.metadata["provider"], "Fake Search");
    }
}
