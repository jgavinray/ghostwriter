//! The MCP surface: six tools over the hemmingway-1 writing model, built
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
use crate::{prompts, styles};

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
/// System prompt for one call: the base house guide plus the selected
/// style's guide block. The house guide always applies; an external
/// standard layers on top of it.
fn system_prompt(base: &str, style: Option<&styles::Style>) -> String {
    match style {
        Some(style) => format!("{base}\n\n{}", style.guide),
        None => base.to_string(),
    }
}

/// Resolve the style for one call: the caller's `style` argument, else the
/// server default when the argument is absent. The retired `ste` boolean is
/// refused with the replacement spelled out — a stale caller must fail
/// loudly, never silently write without the standard it asked for.
fn resolve_style(
    requested: Option<&str>,
    ste_legacy: &serde_json::Value,
    default: &str,
) -> Result<Option<&'static styles::Style>, String> {
    if !ste_legacy.is_null() {
        return Err(format!(
            "the `ste` argument was replaced by `style`: pass style: \"ste\" for \
             ASD-STE100 (valid: {})",
            styles::usage()
        ));
    }
    match requested.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) if id.eq_ignore_ascii_case(styles::NONE_ID) => Ok(None),
        Some(id) => match styles::lookup(id) {
            Some(style) => Ok(Some(style)),
            None => Err(format!(
                "unknown style {id:?}; expected one of: {}",
                styles::usage()
            )),
        },
        None => {
            if default.eq_ignore_ascii_case(styles::NONE_ID) {
                Ok(None)
            } else {
                styles::lookup(default).map(Some).ok_or_else(|| {
                    format!(
                        "the configured default style {default:?} is not registered; \
                         expected one of: {}",
                        styles::usage()
                    )
                })
            }
        }
    }
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
    /// Writing standard on top of the house style: ste (ASD-STE100 Simplified Technical English), google (Google developer documentation style guide), microsoft (Microsoft Writing Style Guide), diataxis (Diátaxis documentation architecture: one document, one mode), or none (house style only). Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Removed: the `ste` boolean argument was replaced by `style`. Calls that still send it are refused with the replacement spelled out.
    #[serde(
        default,
        rename = "ste",
        skip_serializing_if = "serde_json::Value::is_null"
    )]
    #[schemars(skip)]
    pub ste_legacy: serde_json::Value,
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
    /// A short sample of the writer's own prose (two or three paragraphs). The rewrite matches that voice — sentence length, word choice, punctuation, dash rate — and the sample overrides the standard rules where they conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_sample: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Rewrite to obey an external standard on top of the house rules: ste (ASD-STE100), google (Google developer documentation style guide), microsoft (Microsoft Writing Style Guide), or diataxis (keep the text's Diátaxis mode — tutorial, how-to, reference, or explanation — and repair mode drift). Facts still survive verbatim. none keeps the house style alone. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Removed: the `ste` boolean argument was replaced by `style`. Calls that still send it are refused with the replacement spelled out.
    #[serde(
        default,
        rename = "ste",
        skip_serializing_if = "serde_json::Value::is_null"
    )]
    #[schemars(skip)]
    pub ste_legacy: serde_json::Value,
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
    /// A short sample of the author's own prose. Patterns the sample itself exhibits (dash rate, fragments, contractions) are the author's voice, not faults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_sample: Option<String>,
    /// Completion budget in tokens, 1..32768. Defaults to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Also judge the text against an external standard: ste (ASD-STE100), google (Google developer documentation style guide), microsoft (Microsoft Writing Style Guide), or diataxis (name the mode the text aims for and flag mixed modes). none keeps the house faults alone. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Removed: the `ste` boolean argument was replaced by `style`. Calls that still send it are refused with the replacement spelled out.
    #[serde(
        default,
        rename = "ste",
        skip_serializing_if = "serde_json::Value::is_null"
    )]
    #[schemars(skip)]
    pub ste_legacy: serde_json::Value,
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
    /// Compose under an external standard on top of the house style: ste (ASD-STE100), google (Google developer documentation style guide), microsoft (Microsoft Writing Style Guide), or diataxis (pick the one mode the material fits — tutorial, how-to, reference, or explanation — and write only in it). none keeps the house style alone. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Removed: the `ste` boolean argument was replaced by `style`. Calls that still send it are refused with the replacement spelled out.
    #[serde(
        default,
        rename = "ste",
        skip_serializing_if = "serde_json::Value::is_null"
    )]
    #[schemars(skip)]
    pub ste_legacy: serde_json::Value,
    /// Sampling temperature, 0..2. Defaults to the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StyleCheckArgs {
    /// The prose to check. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A file whose contents to check. Mutually exclusive with `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Which registered standard to check mechanically: ste (the default), google, or microsoft. The other registered styles have no deterministic rules and are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
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
        let style = match resolve_style(args.style.as_deref(), &args.ste_legacy, &self.config.style)
        {
            Ok(style) => style,
            Err(e) => return Ok(error_result(e)),
        };
        if let Some(style) = style {
            head.push_str(&format!("\n{}", style.head_line));
        }
        let system = system_prompt(prompts::STYLE_GUIDE.as_str(), style);
        Ok(self
            .render(
                &system,
                &user_prompt(&head, "Source", &body),
                self.clamp_tokens(args.max_tokens, 8192),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Rewrite text so it reads like a careful human wrote it: removes AI-writing patterns (staged contrasts, one-line closers, forced triads, stock AI vocabulary, inflated significance, formatting-by-rule, chatbot residue — Wikipedia's "Signs of AI writing" list), enforces formal mechanics (Oxford comma, restrained em dashes, sentence-case headings), and preserves every fact — prose changes only; code blocks, commands, paths, and URLs stay intact. Optional `voice_sample` (the writer's own prose) matches the rewrite to that voice's register and word choice; em-dash rate preservation is best-effort on the current model, and critique_prose is the authoritative dash-rate judge against the sample. Provide `text` or `path` — exactly one. Run this on documentation drafts (including your own) before shipping them.
    #[tool(
        description = "Rewrite text so it reads like a careful human wrote it: removes AI-writing patterns (staged contrasts, one-line closers, forced triads, stock AI vocabulary, inflated significance, formatting-by-rule, chatbot residue — Wikipedia's \"Signs of AI writing\" list), enforces formal mechanics (Oxford comma, restrained em dashes, sentence-case headings), and preserves every fact — prose changes only; code blocks, commands, paths, and URLs stay intact. Optional `voice_sample` (the writer's own prose) matches the rewrite to that voice's register and word choice; em-dash rate preservation is best-effort on the current model, and critique_prose is the authoritative dash-rate judge against the sample. Provide `text` or `path` — exactly one. Run this on documentation drafts (including your own) before shipping them."
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
        let voice = args
            .voice_sample
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if voice.is_some() {
            // Empirically tuned head pair (kaibo consult job-1 + live A/B,
            // 2026-09-24): Goal + Voice lines in this exact wording are the
            // only configuration observed to preserve the sample's dash
            // rate; later guide-side exception variants measured 0/10.
            // Re-verify against any hemmingway-1 model update; degrade mode
            // is dashes normalized away, which critique_prose still judges
            // correctly against the sample.
            head.push_str(
                "\nGoal: Keep the em dashes; they are part of the author's voice and the writing sample uses them.",
            );
            head.push_str(
                "\nVoice: the writing sample below defines the author's voice; match it and let it override the standard rules where they conflict.",
            );
        }
        if let Some(audience) = args
            .audience
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            head.push_str(&format!("\nAudience: {audience}"));
        }
        let style = match resolve_style(args.style.as_deref(), &args.ste_legacy, &self.config.style)
        {
            Ok(style) => style,
            Err(e) => return Ok(error_result(e)),
        };
        if let Some(style) = style {
            head.push_str(&format!("\n{}", style.head_line));
        }
        // The sample goes before the text: the model reads the voice it must
        // match first, mirroring the humanizer skill's sample-then-text order.
        let user = match voice {
            Some(sample) => format!(
                "{head}\n\nWriting sample (match this voice):\n````text\n{sample}\n````\n\nText to rewrite:\n````text\n{body}\n````"
            ),
            None => user_prompt(&head, "Text to rewrite", &body),
        };
        let system = system_prompt(prompts::REWRITE_GUIDE.as_str(), style);
        Ok(self
            .render(
                &system,
                &user,
                self.clamp_tokens(args.max_tokens, 4096),
                self.temperature(args.temperature),
            )
            .await)
    }

    /// Editorial review of text WITHOUT rewriting it: a numbered list of concrete faults — AI-writing patterns (staged contrasts, one-line closers, forced triads, stock vocabulary, formatting-by-rule) plus filler, puff words, passive voice, vague claims, run-on ideas, missing Oxford comma, em-dash overuse, invented specifics — each quoting the phrase and proposing the local fix. Optionally pass `source` (reference text) to have every factual claim verified against it, and `voice_sample` so patterns the sample itself exhibits are not flagged. Returns exactly CLEAN when there is nothing to fix. Provide `text` or `path` — exactly one.
    #[tool(
        description = "Editorial review of text WITHOUT rewriting it: a numbered list of concrete faults — AI-writing patterns (staged contrasts, one-line closers, forced triads, stock vocabulary, formatting-by-rule) plus filler, puff words, passive voice, vague claims, run-on ideas, missing Oxford comma, em-dash overuse, invented specifics — each quoting the phrase and proposing the local fix. Optionally pass `source` (reference text) to have every factual claim in the reviewed text verified against it, and `voice_sample` so patterns the sample itself exhibits are not flagged. Returns exactly CLEAN when there is nothing to fix. Provide `text` or `path` — exactly one."
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
        let source = args
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if source.is_some() {
            head.push_str(
                " The text makes factual claims; verify every one against the reference source.",
            );
        }
        let voice = args
            .voice_sample
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if voice.is_some() {
            head.push_str(
                " The writing sample defines the author's voice; do not flag a pattern the sample itself exhibits.",
            );
        }
        let style = match resolve_style(args.style.as_deref(), &args.ste_legacy, &self.config.style)
        {
            Ok(style) => style,
            Err(e) => return Ok(error_result(e)),
        };
        if let Some(style) = style {
            head.push_str(style.critique_line);
        }
        let mut user = format!("{head}\n\nText to review:\n````text\n{body}\n````");
        if let Some(sample) = voice {
            user.push_str(&format!(
                "\n\nWriting sample (defines the author's voice):\n````text\n{sample}\n````"
            ));
        }
        if let Some(source) = source {
            user.push_str(&format!("\n\nReference source:\n````text\n{source}\n````"));
        }
        let system = system_prompt(prompts::CRITIQUE_GUIDE.as_str(), style);
        Ok(self
            .render(
                &system,
                &user,
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
        let style = match resolve_style(args.style.as_deref(), &args.ste_legacy, &self.config.style)
        {
            Ok(style) => style,
            Err(e) => return Ok(error_result(e)),
        };
        if let Some(style) = style {
            head.push_str(&format!("\n{}", style.head_line));
        }
        let user = match previous {
            Some(prev) => format!(
                "{head}\n\nMaterial:\n````text\n{body}\n````\n\nPrevious report:\n````text\n{prev}\n````"
            ),
            None => user_prompt(&head, "Material", &body),
        };
        let system = system_prompt(prompts::COMPOSE_GUIDE.as_str(), style);
        Ok(self
            .render(
                &system,
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

    /// Deterministic style check over the registered standards — no model call: `ste` (the default) reports the ASD-STE100 unapproved-word dictionary with approved replacements, semicolons, contractions, Latin abbreviations, and sentences over the STE caps (20 words for an instruction, 25 for description); `google` reports Latin abbreviations, exclamation marks, tone words (please, simply, easily, just, obviously, of course, let's), and internet slang (tl;dr, ymmv, rtfm); `microsoft` reports Latin abbreviations, exclamation marks, please, and the bias-free term list. Code blocks, inline code, commands, paths, and URLs are exempt. Returns JSON {clean, style, findings:[{line, quote, rule, fix}]}; the `ste` report adds `dictionary`, the shipped dictionary's entry count. Run it on technical prose before or instead of a model pass; findings are mechanical, so an empty list does not judge style. Provide `text` or `path` — exactly one.
    #[tool(
        description = "Deterministic style check over the registered standards — no model call: style ste (default) reports the ASD-STE100 unapproved-word dictionary, semicolons, contractions, Latin abbreviations, and sentence-length caps; google reports Latin abbreviations, exclamation marks, tone words (please, simply, easily, just, obviously, of course, let's), and internet slang (tl;dr, ymmv, rtfm); microsoft reports Latin abbreviations, exclamation marks, please, and the bias-free term list. Code blocks, inline code, commands, paths, and URLs are exempt. Returns JSON {clean, style, findings:[{line, quote, rule, fix}]}; the ste report adds dictionary, the shipped dictionary's entry count. Findings are mechanical; an empty list does not judge style. Provide `text` or `path` — exactly one."
    )]
    async fn style_check(
        &self,
        Parameters(args): Parameters<StyleCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        let requested = args
            .style
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("ste");
        let style = if requested.eq_ignore_ascii_case(styles::NONE_ID) {
            return Ok(error_result(format!(
                "the house style has no mechanical rules to check; expected one of: {}",
                styles::checkable()
            )));
        } else {
            match styles::lookup(requested) {
                Some(style) => style,
                None => {
                    return Ok(error_result(format!(
                        "unknown style {requested:?}; expected one of: {}",
                        styles::checkable()
                    )))
                }
            }
        };
        let rules = match style.rules {
            Some(rules) => rules,
            None => {
                return Ok(error_result(format!(
                    "style {:?} has no deterministic checks; the mechanical rules exist for: {}",
                    style.id,
                    styles::checkable()
                )))
            }
        };
        let body = match source_body(args.text.as_deref(), args.path.as_deref()).await {
            Ok(body) => body,
            Err(e) => return Ok(error_result(e)),
        };
        let findings = crate::ste::check_with(&body, rules);
        let mut report = json!({
            "clean": findings.is_empty(),
            "style": style.id,
            "findings": findings,
        });
        if rules.dictionary {
            report["dictionary"] = json!(crate::ste::dictionary_len());
        }
        let content = match ContentBlock::json(&report) {
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
                 - Human-facing prose is finalized by the hemmingway-1 model through these tools: document_code drafts docs from source, compose writes standups, PRDs, one-pagers, announcements, summaries, release notes, postmortems, weekly statuses, and meeting notes from raw material, rewrite_prose rewrites existing text so it reads like a human wrote it, critique_prose reports findings without rewriting (optionally against a reference source).\n\
                 - Draft with document_code, or compose from raw material, then pass the draft through critique_prose (and rewrite_prose for the final pass) before shipping it.\n\
                 - rewrite_prose removes the AI-writing patterns from Wikipedia's \"Signs of AI writing\" (staged contrasts, one-line closers, forced triads, stock vocabulary, inflated significance, formatting-by-rule, chatbot residue) and preserves every fact: prose changes only, code blocks, commands, paths, and URLs stay intact, so it is safe on markdown files.\n\
                 - To humanize with visible checks: rewrite_prose, then critique_prose on the result, then rewrite_prose once more with the critique findings as `goal`.\n\
                 - To match a writer's voice, pass their prose as `voice_sample` to rewrite_prose (and critique_prose when reviewing): the sample steers register and word choice; em-dash rate preservation in rewrite is best-effort, and critique_prose judges dash rate against the sample.\n\
                 - For technical documentation, pass style: \"ste\", \"google\", \"microsoft\", or \"diataxis\" to document_code, compose, rewrite_prose, or critique_prose to write under ASD-STE100, the Google developer documentation style guide, the Microsoft Writing Style Guide, or the Diátaxis documentation architecture; style_check runs the mechanical half of ste, google, and microsoft with no model call. style: \"none\" keeps the house style alone and is the server default when the configuration sets none.\n\
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
            idle_timeout: std::time::Duration::from_millis(500),
            style: "none".into(),
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
                voice_sample: None,
                max_tokens: None,
                style: None,
                ste_legacy: serde_json::Value::Null,
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
                style: None,
                ste_legacy: serde_json::Value::Null,
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
                style: None,
                ste_legacy: serde_json::Value::Null,
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
                style: None,
                ste_legacy: serde_json::Value::Null,
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
                style: None,
                ste_legacy: serde_json::Value::Null,
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
                style: None,
                ste_legacy: serde_json::Value::Null,
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

    #[tokio::test]
    async fn style_check_requires_source() {
        let s = server();
        assert_error_result(
            s.style_check(Parameters(StyleCheckArgs {
                text: None,
                path: None,
                style: None,
            }))
            .await,
            "provide",
        )
        .await;
    }

    #[tokio::test]
    async fn style_check_defaults_to_ste_and_reports_planted_violations() {
        let s = server();
        let result = s
            .style_check(Parameters(StyleCheckArgs {
                text: Some(
                    "The operator utilizes the gauge to initiate the pump; the reading is stable.\nSet the valve to the open position.\n".into(),
                ),
                path: None,
                style: None,
            }))
            .await
            .expect("no protocol error");
        assert_ne!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("\"style\":\"ste\""), "{text}");
        assert!(text.contains("\"clean\":false"), "{text}");
        assert!(text.contains("utilize"), "{text}");
        assert!(text.contains("initiate"), "{text}");
        assert!(text.contains("ste-semicolon"), "{text}");
    }

    #[tokio::test]
    async fn style_check_clean_prose() {
        let s = server();
        let result = s
            .style_check(Parameters(StyleCheckArgs {
                text: Some("Turn the handle two full turns. Tighten the nut to 20 Nm.".into()),
                path: None,
                style: None,
            }))
            .await
            .expect("no protocol error");
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("\"clean\":true"), "{text}");
    }

    #[tokio::test]
    async fn style_check_runs_the_google_rule_set() {
        let s = server();
        let result = s
            .style_check(Parameters(StyleCheckArgs {
                text: Some("Simply deploy the app, e.g. on Fridays.".into()),
                path: None,
                style: Some("google".into()),
            }))
            .await
            .expect("no protocol error");
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("\"style\":\"google\""), "{text}");
        assert!(text.contains("google-tone"), "{text}");
        assert!(text.contains("google-latin"), "{text}");
    }

    #[tokio::test]
    async fn style_check_refuses_styles_without_mechanical_rules() {
        let s = server();
        assert_error_result(
            s.style_check(Parameters(StyleCheckArgs {
                text: Some("anything".into()),
                path: None,
                style: Some("diataxis".into()),
            }))
            .await,
            "no deterministic checks",
        )
        .await;
        assert_error_result(
            s.style_check(Parameters(StyleCheckArgs {
                text: Some("anything".into()),
                path: None,
                style: Some("none".into()),
            }))
            .await,
            "house style has no mechanical rules",
        )
        .await;
        assert_error_result(
            s.style_check(Parameters(StyleCheckArgs {
                text: Some("anything".into()),
                path: None,
                style: Some("chicago".into()),
            }))
            .await,
            "unknown style",
        )
        .await;
    }

    #[tokio::test]
    async fn unknown_style_lists_valid_styles() {
        let s = server();
        assert_error_result(
            s.document_code(Parameters(DocumentCodeArgs {
                code: Some("fn main() {}".into()),
                path: None,
                kind: None,
                audience: None,
                notes: None,
                max_tokens: None,
                style: Some("chicago".into()),
                ste_legacy: serde_json::Value::Null,
                temperature: None,
            }))
            .await,
            "diataxis",
        )
        .await;
    }

    /// The retired `ste` boolean must fail loudly with the replacement
    /// named — never be ignored, which would silently drop the standard.
    #[tokio::test]
    async fn legacy_ste_argument_is_refused() {
        let s = server();
        assert_error_result(
            s.rewrite_prose(Parameters(RewriteProseArgs {
                text: Some("The system utilizes the pump.".into()),
                path: None,
                goal: None,
                audience: None,
                voice_sample: None,
                max_tokens: None,
                style: None,
                ste_legacy: serde_json::json!(true),
                temperature: None,
            }))
            .await,
            "`style`",
        )
        .await;
    }

    /// The config `style` default applies when the caller omits the
    /// argument; an explicit argument always wins; `none` opts out of even
    /// a server-configured standard; unknown ids list the valid ones.
    #[test]
    fn style_resolution_prefers_the_caller() {
        let ste_marker = "ASD-STE100 mode (Simplified Technical English)";
        let google_marker = "Google developer documentation style";
        assert!(system_prompt(
            "BASE",
            resolve_style(None, &serde_json::Value::Null, "ste").unwrap()
        )
        .contains(ste_marker));
        assert!(system_prompt(
            "BASE",
            resolve_style(None, &serde_json::Value::Null, "google").unwrap()
        )
        .contains(google_marker));
        assert_eq!(
            system_prompt(
                "BASE",
                resolve_style(None, &serde_json::Value::Null, "none").unwrap()
            ),
            "BASE"
        );
        assert_eq!(
            system_prompt(
                "BASE",
                resolve_style(Some("none"), &serde_json::Value::Null, "ste").unwrap()
            ),
            "BASE"
        );
        assert!(system_prompt(
            "BASE",
            resolve_style(Some("STE"), &serde_json::Value::Null, "none").unwrap()
        )
        .contains(ste_marker));
        let err = resolve_style(Some("chicago"), &serde_json::Value::Null, "none").unwrap_err();
        assert!(err.contains("google") && err.contains("diataxis"), "{err}");
        let err = resolve_style(None, &serde_json::json!(true), "none").unwrap_err();
        assert!(err.contains("`style`"), "{err}");
        // A default that never reached the registry (reachable only by
        // constructing Config directly) must refuse, not silently drop.
        let err = resolve_style(None, &serde_json::Value::Null, "chicago").unwrap_err();
        assert!(err.contains("not registered"), "{err}");
    }
    /// The retire must survive the real consumer path: rmcp hands the
    /// handler JSON arguments through serde. A wrong rename, a field made
    /// `skip`, or a set `ste` read as "absent" would let a stale caller
    /// silently lose its standard — the one outcome the invariant forbids.
    #[test]
    fn legacy_ste_refusal_survives_deserialization() {
        let args: RewriteProseArgs = serde_json::from_str(r#"{"text":"x","ste":true}"#).unwrap();
        let err = resolve_style(args.style.as_deref(), &args.ste_legacy, "none").unwrap_err();
        assert!(err.contains("replaced by `style`"), "{err}");

        // An explicit JSON null reads as absent, the standard convention
        // for an unset optional argument. Anything the caller actually set
        // — including the old `false` opt-out — must refuse loudly.
        let args: RewriteProseArgs = serde_json::from_str(r#"{"text":"x","ste":null}"#).unwrap();
        assert!(args.ste_legacy.is_null());
        assert!(resolve_style(args.style.as_deref(), &args.ste_legacy, "none").is_ok());
        let args: RewriteProseArgs = serde_json::from_str(r#"{"text":"x","ste":false}"#).unwrap();
        assert!(resolve_style(args.style.as_deref(), &args.ste_legacy, "none").is_err());

        // A modern call deserializes with the retired field truly absent.
        let args: RewriteProseArgs =
            serde_json::from_str(r#"{"text":"x","style":"none"}"#).unwrap();
        assert!(args.ste_legacy.is_null());

        // The advertised schema must not offer the retired argument.
        let schema = serde_json::to_value(schemars::schema_for!(RewriteProseArgs)).unwrap();
        let props = schema.get("properties").unwrap();
        assert!(props.get("style").is_some(), "{schema}");
        assert!(props.get("ste").is_none(), "{schema}");
    }
}
