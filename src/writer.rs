//! The MCP surface: five tools over the hemmingway-1 writing model, built
//! with rmcp's `#[tool_router]`/`#[tool]` macros (typed arguments, schemars
//! schemas, stdio transport wiring done by the SDK).
//!
//! Tool-level failures (bad arguments, unreadable files, model errors) are
//! reported as `CallToolResult::error` — an `isError: true` result the
//! calling agent can read — never as a JSON-RPC protocol error.

use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::Client;
use crate::config::{Config, MAX_SOURCE_BYTES, MAX_TOKENS_CAP};
use crate::prompts;

/// Source/payload text arriving from a tool argument is fenced with four
/// backticks; anything shorter collides with code blocks inside the
/// documentation source far too often. The label is per-tool — a small
/// model reads the framing noun, and "Source:" before prose invites it to
/// treat prose as code.
fn user_prompt(head: &str, label: &str, body: &str) -> String {
    format!("{head}\n\n{label}:\n````text\n{body}\n````")
}

/// A tool-level failure as a normal `isError` result the calling agent can
/// read — not a JSON-RPC protocol error.
fn error_result(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

/// Resolve an inline payload (`code` / `text`) or a file `path` into one
/// body. Exactly one of the two is mandatory; disk reads are capped with a
/// VISIBLE marker so the model can report truncation instead of documenting
/// half a file as if it were whole.
async fn source_body(inline: Option<&str>, path: Option<&str>) -> Result<String, String> {
    match (
        inline.map(str::trim).filter(|s| !s.is_empty()),
        path.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(_), Some(_)) => Err("provide the inline argument or `path`, not both".into()),
        (None, None) => Err("provide the inline argument or `path`".into()),
        (Some(text), None) => Ok(text.to_string()),
        (None, Some(p)) => {
            let bytes = tokio::fs::read(p)
                .await
                .map_err(|e| format!("reading {p} failed: {e}"))?;
            let truncated = bytes.len() > MAX_SOURCE_BYTES;
            let slice = if truncated {
                &bytes[..MAX_SOURCE_BYTES]
            } else {
                &bytes[..]
            };
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                text.push_str(&format!(
                    "\n\n[ghostwriter: source truncated at {MAX_SOURCE_BYTES} bytes of {}; the rest was not sent to the model]\n",
                    bytes.len()
                ));
            }
            Ok(text)
        }
    }
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DocumentCodeArgs {
    /// The source code to document. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// A file to read as the source. Mutually exclusive with `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Document type: overview | api | readme | function | cli | config | changelog. Defaults to api.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Who reads the document. Defaults to developers who can read the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Extra author instructions the model must follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature, 0..2. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RewriteProseArgs {
    /// The prose to rewrite. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A file whose contents to rewrite. Mutually exclusive with `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Editorial intent, e.g. "tighten for a release note".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// Who reads the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature, 0..2. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CritiqueProseArgs {
    /// The prose to review. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A file whose contents to review. Mutually exclusive with `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Reference source the text's factual claims must be checked against. Optional; without it the review can only judge internal vagueness, not correctness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature, 0..2. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeArgs {
    /// The raw material: bullets, ticket exports, commit logs, notes, metrics. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    /// A file to read as the raw material. Mutually exclusive with `material`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Document type: standup | prd | one-pager | announcement | summary | release-notes | postmortem | weekly-status | meeting-notes. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Who reads the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// The document date (e.g. "2026-09-21"). The model has no clock; supply it for standups and dated reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// The document author or team name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Target length, e.g. "one page", "under 200 words", "3 bullets per section".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<String>,
    /// Override the kind's default section list, e.g. ["Yesterday", "Today", "Blockers"].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<String>>,
    /// The previous report of the same kind, so the document can report what changed since it (multi-day standup deltas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Extra author instructions the model must follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature, 0..2. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Clone)]
pub struct WritingServer {
    config: Arc<Config>,
    client: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl WritingServer {
    pub fn new(config: Config) -> Result<Self, String> {
        Ok(Self {
            client: Arc::new(Client::new(&config)?),
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        })
    }

    fn clamp_tokens(&self, requested: Option<u32>, default: u32) -> u32 {
        requested.unwrap_or(default).clamp(1, MAX_TOKENS_CAP)
    }

    fn temperature(&self, requested: Option<f32>) -> f32 {
        requested
            .map(|t| t.clamp(0.0, 2.0))
            .unwrap_or(self.config.temperature)
    }

    /// Run one completion and wrap the result. Tool-level failures —
    /// argument problems, unreadable files, model errors — travel as
    /// `CallToolResult::error` (`isError: true`) per the MCP spec, never as
    /// JSON-RPC protocol errors. A length-truncated answer gets a visible
    /// marker so the caller raises `max_tokens` or shrinks the source
    /// rather than shipping half a document.
    async fn render(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        temperature: f32,
    ) -> CallToolResult {
        let completion = match self
            .client
            .complete(&self.config, system, user, max_tokens, temperature)
            .await
        {
            Ok(completion) => completion,
            Err(e) => return error_result(e),
        };
        let text = if completion.truncated {
            format!(
                "{}\n\n[ghostwriter: output truncated at max_tokens={max_tokens} — re-call with a larger max_tokens or a smaller source]",
                completion.text
            )
        } else {
            completion.text
        };
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    /// Draft professional documentation for source code with the hemmingway-1 writing model: formal register, Oxford comma, active voice, no filler or puff words. Provide `code` (source text) or `path` (file to read) — exactly one. `kind` selects the document type (default api). The model describes only what the source actually does; review output against the code before publishing.
    #[tool(
        description = "Draft professional documentation for source code with the hemmingway-1 writing model: formal register, Oxford comma, active voice, no filler or puff words. Provide `code` (source text) or `path` (file to read) — exactly one. `kind` selects the document type (default api). The model describes only what the source actually does; review output against the code before publishing."
    )]
    async fn document_code(
        &self,
        Parameters(args): Parameters<DocumentCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match source_body(args.code.as_deref(), args.path.as_deref()).await {
            Ok(body) => body,
            Err(e) => return Ok(error_result(e)),
        };
        let kind = args.kind.as_deref().unwrap_or("api").trim();
        let instruction = match prompts::kind_instruction(kind) {
            Some(instruction) => instruction,
            None => {
                return Ok(error_result(format!(
                    "unknown kind {kind:?}; expected one of: overview, api, readme, function, cli, config, changelog"
                )))
            }
        };
        let mut head = format!("Task: {instruction}");
        match args
            .audience
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(audience) => head.push_str(&format!("\nAudience: {audience}")),
            None => head.push_str("\nAudience: developers who can read the source."),
        }
        if let Some(notes) = args
            .notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nAuthor notes (follow them): {notes}"));
        }
        Ok(self
            .render(
                prompts::STYLE_GUIDE,
                &user_prompt(&head, "Source", &body),
                self.clamp_tokens(args.max_tokens, 8192),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Rewrite text to a professional standard: filler, puff words, softeners, passive voice, vague claims, and multi-idea sentences are removed; formal mechanics (Oxford comma, restrained em dashes, sentence-case headings) are enforced. Facts and format are preserved; nothing is added. Provide `text` or `path` — exactly one. Run this on documentation drafts (including your own) before shipping them.
    #[tool(
        description = "Rewrite text to a professional standard: filler, puff words, softeners, passive voice, vague claims, and multi-idea sentences are removed; formal mechanics (Oxford comma, restrained em dashes, sentence-case headings) are enforced. Facts and format are preserved; nothing is added. Provide `text` or `path` — exactly one. Run this on documentation drafts (including your own) before shipping them."
    )]
    async fn rewrite_prose(
        &self,
        Parameters(args): Parameters<RewriteProseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match source_body(args.text.as_deref(), args.path.as_deref()).await {
            Ok(body) => body,
            Err(e) => return Ok(error_result(e)),
        };
        let mut head = String::from("Rewrite the text below.");
        if let Some(goal) = args
            .goal
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nGoal: {goal}"));
        }
        if let Some(audience) = args
            .audience
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nAudience: {audience}"));
        }
        Ok(self
            .render(
                prompts::REWRITE_GUIDE,
                &user_prompt(&head, "Text to rewrite", &body),
                self.clamp_tokens(args.max_tokens, 4096),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Editorial review of text WITHOUT rewriting it: a numbered list of concrete faults (filler, puff words, passive voice, vague claims, run-on ideas, missing Oxford comma, em-dash overuse, invented specifics), each quoting the phrase and proposing the local fix. Optionally pass `source` to have every factual claim verified against it. Returns exactly CLEAN when there is nothing to fix. Provide `text` or `path` — exactly one.
    #[tool(
        description = "Editorial review of text WITHOUT rewriting it: a numbered list of concrete faults (filler, puff words, passive voice, vague claims, run-on ideas, missing Oxford comma, em-dash overuse, invented specifics), each quoting the phrase and proposing the local fix. Optionally pass `source` (reference text) to have every factual claim in the reviewed text verified against it. Returns exactly CLEAN when there is nothing to fix. Provide `text` or `path` — exactly one."
    )]
    async fn critique_prose(
        &self,
        Parameters(args): Parameters<CritiqueProseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match source_body(args.text.as_deref(), args.path.as_deref()).await {
            Ok(body) => body,
            Err(e) => return Ok(error_result(e)),
        };
        let mut head = String::from("Review the text below.");
        if let Some(source) = args
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(
                " The text makes factual claims; verify every one against the reference source.",
            );
            return Ok(self
                .render(
                    prompts::CRITIQUE_GUIDE,
                    &format!(
                        "{head}\n\nText to review:\n````text\n{body}\n````\n\nReference source:\n````text\n{source}\n````"
                    ),
                    self.clamp_tokens(args.max_tokens, 4096),
                    self.temperature(args.temperature),
                )
                .await);
        }
        Ok(self
            .render(
                prompts::CRITIQUE_GUIDE,
                &user_prompt(&head, "Text to review", &body),
                self.clamp_tokens(args.max_tokens, 4096),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Compose a formal document from raw material — bullet points, ticket exports, commit logs, notes, metrics. Kinds: standup, prd, one-pager, announcement, summary, release-notes, postmortem, weekly-status, meeting-notes. Every fact in the output is traceable to the material; gaps are admitted, never invented. Optional plumbing: `date`, `author`, `length`, `sections`, `previous` (delta reports). Provide `material` or `path` — exactly one; `kind` is required.
    #[tool(
        description = "Compose a formal document from raw material — bullet points, ticket exports, commit logs, notes, metrics. Kinds: standup, prd, one-pager, announcement, summary, release-notes, postmortem, weekly-status, meeting-notes. Formal register with enforced mechanics (Oxford comma, restrained em dashes); every fact in the output is traceable to the material, and gaps are admitted rather than invented. Optional plumbing: `date`, `author`, `length` target, `sections` override, and `previous` (the last report of the same kind, for delta reports). Provide `material` or `path` — exactly one; `kind` is required."
    )]
    async fn compose(
        &self,
        Parameters(args): Parameters<ComposeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match source_body(args.material.as_deref(), args.path.as_deref()).await {
            Ok(body) => body,
            Err(e) => return Ok(error_result(e)),
        };
        let kind = match args
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(kind) => kind,
            None => {
                return Ok(error_result(format!(
                    "`kind` is required; expected one of: {}",
                    prompts::COMPOSE_KINDS.join(", ")
                )))
            }
        };
        let instruction = match prompts::compose_kind_instruction(kind) {
            Some(instruction) => instruction,
            None => {
                return Ok(error_result(format!(
                    "unknown kind {kind:?}; expected one of: {}",
                    prompts::COMPOSE_KINDS.join(", ")
                )))
            }
        };
        let mut head = format!("Task: {instruction}");
        match args
            .audience
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(audience) => head.push_str(&format!("\nAudience: {audience}")),
            None => head.push_str("\nAudience: the team and stakeholders named in the material."),
        }
        if let Some(date) = args
            .date
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nDate: {date}"));
        }
        if let Some(author) = args
            .author
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nAuthor: {author}"));
        }
        if let Some(length) = args
            .length
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nLength: {length}"));
        }
        if let Some(sections) = args.sections.as_deref().filter(|s| !s.is_empty()) {
            head.push_str(&format!(
                "\nSections (these override the kind's default sections; use exactly these, in this order): {}",
                sections.join(", ")
            ));
        }
        if let Some(notes) = args
            .notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nAuthor notes (follow them): {notes}"));
        }
        let previous = args
            .previous
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let user = match previous {
            Some(prev) => format!(
                "{head}\n\nMaterial:\n````text\n{body}\n````\n\nPrevious report:\n````text\n{prev}\n````"
            ),
            None => user_prompt(&head, "Material", &body),
        };
        Ok(self
            .render(
                prompts::COMPOSE_GUIDE,
                &user,
                self.clamp_tokens(args.max_tokens, 8192),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Check that the hemmingway-1 server is reachable and serving the configured model id. Call this first when the writing tools fail.
    #[tool(
        description = "Check that the hemmingway-1 server is reachable and serving the configured model id. Call this first when the writing tools fail."
    )]
    async fn model_health(&self) -> Result<CallToolResult, McpError> {
        let base_url = self.config.base_url.clone();
        let model = self.config.model.clone();
        let health = match self.client.list_models(&self.config).await {
            Ok(served) => json!({
                "reachable": true,
                "base_url": base_url,
                "model": model,
                "served": served,
                "model_present": served.iter().any(|id| id == &model),
            }),
            Err(e) => json!({
                "reachable": false,
                "base_url": base_url,
                "model": model,
                "error": e,
            }),
        };
        let content = match ContentBlock::json(&health) {
            Ok(content) => content,
            Err(e) => return Ok(error_result(e.message.into_owned())),
        };
        Ok(CallToolResult::success(vec![content]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WritingServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ghostwriter", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "## Writing protocol (MCP: ghostwriter)\n\
                 - Human-facing prose is finalized by the hemmingway-1 model through these tools: document_code drafts docs from source, compose writes standups, PRDs, one-pagers, announcements, summaries, release notes, postmortems, weekly statuses, and meeting notes from raw material, rewrite_prose de-slops existing text, critique_prose reports findings without rewriting (optionally against a reference source).\n\
                 - Draft with document_code, or compose from raw material, then pass the draft through critique_prose (and rewrite_prose for the final pass) before shipping it.\n\
                 - critique_prose returns exactly CLEAN when there is nothing to fix.\n\
                 - When the writing tools fail, call model_health first; it names the endpoint and the served model ids.\n",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> WritingServer {
        WritingServer::new(Config {
            base_url: "http://127.0.0.1:9".into(),
            model: "test-model".into(),
            temperature: 0.3,
            timeout: std::time::Duration::from_secs(1),
        })
        .unwrap()
    }

    /// A refusal must be a normal `isError` result — never a JSON-RPC
    /// protocol error (MCP spec: tool failures travel inside results).
    async fn assert_error_result(result: Result<CallToolResult, McpError>, needle: &str) {
        let result = result.expect("handler must not fail at the protocol level");
        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains(needle), "message {text:?} lacks {needle:?}");
    }

    #[tokio::test]
    async fn missing_source_is_an_error_result() {
        let s = server();
        assert_error_result(
            s.rewrite_prose(Parameters(RewriteProseArgs {
                text: None,
                path: None,
                goal: None,
                audience: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "provide",
        )
        .await;
    }

    #[tokio::test]
    async fn both_sources_rejected() {
        let s = server();
        assert_error_result(
            s.document_code(Parameters(DocumentCodeArgs {
                code: Some("fn main() {}".into()),
                path: Some("/tmp/x.rs".into()),
                kind: None,
                audience: None,
                notes: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "not both",
        )
        .await;
    }

    #[tokio::test]
    async fn unknown_kind_lists_valid_kinds() {
        let s = server();
        assert_error_result(
            s.document_code(Parameters(DocumentCodeArgs {
                code: Some("fn main() {}".into()),
                path: None,
                kind: Some("poem".into()),
                audience: None,
                notes: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "changelog",
        )
        .await;
    }

    #[tokio::test]
    async fn compose_requires_material() {
        let s = server();
        assert_error_result(
            s.compose(Parameters(ComposeArgs {
                material: None,
                path: None,
                kind: Some("standup".into()),
                audience: None,
                date: None,
                author: None,
                length: None,
                sections: None,
                previous: None,
                notes: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "provide",
        )
        .await;
    }

    #[tokio::test]
    async fn compose_unknown_kind_lists_valid_kinds() {
        let s = server();
        assert_error_result(
            s.compose(Parameters(ComposeArgs {
                material: Some("- fixed the login bug".into()),
                path: None,
                kind: Some("sonnet".into()),
                audience: None,
                date: None,
                author: None,
                length: None,
                sections: None,
                previous: None,
                notes: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "announcement",
        )
        .await;
    }

    #[tokio::test]
    async fn compose_without_kind_is_an_error_result() {
        let s = server();
        assert_error_result(
            s.compose(Parameters(ComposeArgs {
                material: Some("- fixed the login bug".into()),
                path: None,
                kind: None,
                audience: None,
                date: None,
                author: None,
                length: None,
                sections: None,
                previous: None,
                notes: None,
                max_tokens: None,
                temperature: None,
            }))
            .await,
            "`kind` is required",
        )
        .await;
    }

    #[tokio::test]
    async fn source_reads_files_with_cap() {
        let dir = std::env::temp_dir().join("ghostwriter-writer-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("src.txt");
        std::fs::write(&file, "fn main() {}").unwrap();
        let body = source_body(None, file.to_str()).await.unwrap();
        assert_eq!(body, "fn main() {}");

        let missing = source_body(None, dir.join("nope").to_str()).await;
        assert!(missing.unwrap_err().contains("reading"));
    }
}
