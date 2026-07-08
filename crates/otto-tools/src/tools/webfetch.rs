//! The `webfetch` tool — a port of opencode
//! `packages/opencode/src/tool/webfetch.ts`.
//!
//! Fetches a URL with reqwest (rustls), honoring the timeout and
//! `ctx.abort`, then returns the body as markdown (HTML converted via
//! `html2md`), plain text (tags stripped), or raw HTML (`webfetch.ts:129-152`).
//! Asks the `webfetch` permission before fetching (`webfetch.ts:39-48`) and
//! enforces a 5MB cap (`webfetch.ts:9`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB (webfetch.ts:9)
const DEFAULT_TIMEOUT_SECS: u64 = 30; // webfetch.ts:10
const MAX_TIMEOUT_SECS: u64 = 120; // webfetch.ts:11

#[derive(Debug, Deserialize)]
struct WebFetchParams {
    url: String,
    #[serde(default = "default_format")]
    format: String,
    #[serde(default)]
    timeout: Option<u64>,
}

fn default_format() -> String {
    "markdown".to_string()
}

/// The `webfetch` tool (webfetch.ts:24).
#[derive(Debug, Default, Clone, Copy)]
pub struct WebFetchTool;

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn id(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/webfetch.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch content from" },
                "format": {
                    "type": "string",
                    "enum": ["text", "markdown", "html"],
                    "description": "The format to return the content in. Defaults to markdown."
                },
                "timeout": { "type": "number", "description": "Optional timeout in seconds (max 120)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: WebFetchParams = decode_args(self.id(), args)?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(ToolError::Execution(
                "URL must start with http:// or https://".to_string(),
            ));
        }
        if !matches!(params.format.as_str(), "text" | "markdown" | "html") {
            return Err(ToolError::Execution(format!(
                "Invalid format: {} (expected text, markdown, or html)",
                params.format
            )));
        }

        ctx.permission
            .ask(PermissionRequest {
                permission: "webfetch".to_string(),
                patterns: vec![params.url.clone()],
                always: vec!["*".to_string()],
                metadata: serde_json::json!({
                    "url": params.url,
                    "format": params.format,
                    "timeout": params.timeout,
                }),
            })
            .await?;

        let timeout = Duration::from_secs(
            params
                .timeout
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS),
        );

        let accept = match params.format.as_str() {
            "markdown" => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            _ => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
            }
        };

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let request = client
            .get(&params.url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
            )
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9");

        // Race the request against cooperative cancellation (webfetch.ts:92).
        let response = tokio::select! {
            _ = ctx.abort.cancelled() => return Err(ToolError::Aborted),
            r = request.send() => r.map_err(|e| ToolError::Execution(e.to_string()))?,
        };

        if !response.status().is_success() {
            return Err(ToolError::Execution(format!(
                "Request failed with status {}",
                response.status()
            )));
        }

        if let Some(len) = response.content_length()
            && len as usize > MAX_RESPONSE_SIZE
        {
            return Err(ToolError::Execution(
                "Response too large (exceeds 5MB limit)".to_string(),
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let title = format!("{} ({content_type})", params.url);

        let bytes = tokio::select! {
            _ = ctx.abort.cancelled() => return Err(ToolError::Aborted),
            b = response.bytes() => b.map_err(|e| ToolError::Execution(e.to_string()))?,
        };
        if bytes.len() > MAX_RESPONSE_SIZE {
            return Err(ToolError::Execution(
                "Response too large (exceeds 5MB limit)".to_string(),
            ));
        }

        let content = String::from_utf8_lossy(&bytes).into_owned();
        let is_html = content_type.contains("text/html");

        let output = match params.format.as_str() {
            "markdown" if is_html => convert_html_to_markdown(&content),
            "text" if is_html => extract_text_from_html(&content),
            // html, or non-HTML content: return raw.
            _ => content,
        };

        Ok(ExecuteResult::new(title, output))
    }
}

/// Convert HTML to markdown (`convertHTMLToMarkdown`, webfetch.ts:182-192`).
fn convert_html_to_markdown(html: &str) -> String {
    html2md::parse_html(html)
}

/// Strip tags, skipping `script`/`style`/`noscript`/`iframe`/`object`/`embed`
/// content (`extractTextFromHTML`, webfetch.ts:158-180`).
fn extract_text_from_html(html: &str) -> String {
    const SKIP: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];
    let mut out = String::new();
    let mut skip_depth = 0usize;
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find the end of the tag.
            let Some(rel) = html[i..].find('>') else {
                break;
            };
            let tag = &html[i + 1..i + rel];
            let closing = tag.starts_with('/');
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if SKIP.contains(&name.as_str()) {
                if closing {
                    skip_depth = skip_depth.saturating_sub(1);
                } else if !tag.ends_with('/') {
                    skip_depth += 1;
                }
            }
            i += rel + 1;
        } else {
            let Some(rel) = html[i..].find('<') else {
                if skip_depth == 0 {
                    out.push_str(&html[i..]);
                }
                break;
            };
            if skip_depth == 0 {
                out.push_str(&html[i..i + rel]);
            }
            i += rel;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HTML: &str = "<html><head><style>.x{color:red}</style></head><body><h1>Title</h1><p>Hello <b>world</b></p><script>ignore()</script></body></html>";

    async fn serve(body: &str, content_type: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), content_type),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn markdown_conversion() {
        let server = serve(HTML, "text/html").await;
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let res = WebFetchTool
            .execute(
                serde_json::json!({ "url": server.uri(), "format": "markdown" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("Title"));
        assert!(res.output.contains("world"));
        assert!(!res.output.contains("<h1>"));
    }

    #[tokio::test]
    async fn text_strips_tags_and_scripts() {
        let server = serve(HTML, "text/html").await;
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let res = WebFetchTool
            .execute(
                serde_json::json!({ "url": server.uri(), "format": "text" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("Title"));
        assert!(res.output.contains("Hello"));
        assert!(!res.output.contains("ignore()"));
        assert!(!res.output.contains("color:red"));
    }

    #[tokio::test]
    async fn html_returns_raw() {
        let server = serve(HTML, "text/html").await;
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let res = WebFetchTool
            .execute(
                serde_json::json!({ "url": server.uri(), "format": "html" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("<h1>Title</h1>"));
    }

    #[tokio::test]
    async fn bad_scheme_errors() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = WebFetchTool
            .execute(serde_json::json!({ "url": "ftp://x" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[tokio::test]
    async fn abort_before_fetch_yields_aborted() {
        let server = serve(HTML, "text/html").await;
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let ctx = ToolContext::builder(std::env::temp_dir())
            .abort(token)
            .build();
        let err = WebFetchTool
            .execute(serde_json::json!({ "url": server.uri() }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Aborted));
    }
}
