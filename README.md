# ghostwriter

A single-binary MCP server that produces written documentation through a
locally served writing model. Six tools — `document_code`, `rewrite_prose`,
`critique_prose`, `compose`, `ste_check`, and `model_health` — carry the
whole surface, and every model-backed one of them enforces one house style:
formal register, Oxford comma, active voice, no filler, no puff words, no
invented facts — and the rewrite and review surfaces remove the AI-writing
patterns from Wikipedia's "Signs of AI writing" list (the pattern set behind
the blader/humanizer skill). Any model-backed tool can additionally compose
to ASD-STE100 Simplified Technical English with `ste: true`.

## Why it exists

Coding agents draft prose as a side effect of coding, and their drafts read
like LLM output: hedged, promotional, padded with filler. The prose that
reaches a human — READMEs, API references, standups, release notes — should be
written by a model whose only job is writing, under rules that reject slop.
ghostwriter is that model's tool surface. It exists to solve three problems:

1. Style drift. Every draft passes through the same enforced mechanics and
   banned-vocabulary lists, so output is consistent regardless of which agent
   drafted it.
2. Invented specifics. The drafting guides require every documented signature,
   flag, or default to be supported by the provided source, and `compose`
   admits gaps instead of guessing at them.
3. Data locality. Requests go to a writing model on your own network. Source
   code and prose never leave it.

## The tools

| Tool | Input | Output |
| --- | --- | --- |
| `document_code` | Source code (inline or `path`), a document kind, optional audience and notes | A README, API reference, overview, CLI or config doc, changelog entry, or doc-comment |
| `rewrite_prose` | Prose (inline or `path`), optional goal, audience, `voice_sample`, and `ste` | The same text with AI-writing patterns, filler, puff words, softeners, and passive voice removed; facts, code blocks, commands, and paths preserved. With `voice_sample`, the rewrite matches the writer's register and word choice (em-dash rate is best-effort; `critique_prose` judges it against the sample). With `ste: true`, also composes to ASD-STE100 (approved general vocabulary, one idea per sentence, no semicolons or contractions) |
| `critique_prose` | Prose (inline or `path`), optional reference `source`, `voice_sample`, and `ste` | A numbered fault list (AI-writing patterns plus house faults, plus STE faults when `ste: true`), or exactly `CLEAN` |
| `compose` | Raw material (inline or `path`), a kind, optional date, author, length, sections, previous, and `ste` | A standup, PRD, one-pager, announcement, summary, release notes, postmortem, weekly status, or meeting notes; `ste: true` composes it in ASD-STE100 |
| `ste_check` | Prose (inline or `path`) | Deterministic ASD-STE100 findings with no model call: JSON `{clean, dictionary, findings:[{line, quote, rule, fix}]}` — unapproved general vocabulary with its approved replacement (against the embedded STE100 Issue 8 dictionary), semicolons, contractions, Latin abbreviations, and sentences over the STE caps (20 words for an instruction, 25 for description). Code blocks, inline code, commands, paths, and URLs are exempt. Findings are mechanical: an empty list does not judge style |
| `model_health` | Nothing | Whether the writing-model server is reachable and serving the configured model |

Tool failures travel as `isError` results the calling agent can read, never as
protocol errors. Truncated output carries a visible marker so half a document
is never mistaken for a whole one.

## Simplified Technical English (ASD-STE100)

Pass `ste: true` to `document_code`, `rewrite_prose`, `critique_prose`, or
`compose` and the system prompt gains the ASD-STE100 writing rules on top of
the house style: approved general vocabulary from the controlled dictionary,
command-form instructions, one idea per sentence, no semicolons, no
contractions, no Latin abbreviations, approved tenses. `ste_check` runs the
mechanical half without a model call — dictionary lookups plus the length,
semicolon, contraction, and Latin checks — so an agent can verify a document
before or after the model pass. The rule text is a summary of the
specification and the dictionary carries the word-level replacement data
(word, part of speech, approved replacement); the ASD manual itself is not
duplicated in this repository.

## Building

Rust (stable) is the only requirement; the result is one static binary for
macOS and Linux alike.

```sh
cargo build --release
```

The binary is `target/release/ghostwriter`.

## Configuration

Configuration resolves once at startup, in layers from lowest to highest:

1. Built-in defaults.
2. The config file: `~/.config/ghostwriter/config.toml` — the same path on
   macOS and Linux — or the file named by `--config <path>`.
3. Environment variables: `GHOSTWRITER_BASE_URL`, `GHOSTWRITER_MODEL`,
   `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`,
   `GHOSTWRITER_IDLE_TIMEOUT_SECS`.

A missing default config file is fine; the defaults stand. A `--config` path
that does not exist is a startup error. Unknown keys in the config file are a
startup error, so a typo never silently reverts a setting to its default.

```toml
# ~/.config/ghostwriter/config.toml
base_url = "http://hyper03:8002/v1"
model = "hemmingway-1"
temperature = 0.3
timeout_secs = 900
idle_timeout_secs = 60
```

`timeout_secs` bounds one whole generation attempt, queueing included — a
busy engine holds a queued request in silence until its first token.
`idle_timeout_secs` kills a stream only after it began emitting and then
went silent. A timed-out or stalled request is never resent — the server
is already working on it. Resends cover only
connection failures, 429, 5xx, and empty completions. The MCP client's
per-server timeout must exceed `timeout_secs` or the client reports a
transport timeout before the server can return a readable error.

The built-in defaults point at the fleet's writing-model server
(`http://hyper03:8002/v1`, model `hemmingway-1`); an install anywhere else
must set `base_url` and `model`, in the file or the environment.

Verify resolution without starting the server:

```sh
target/release/ghostwriter --self-check
```

Register the server with an MCP client:

```json
{
  "mcpServers": {
    "ghostwriter": {
      "command": "/usr/local/bin/ghostwriter",
      "timeout": 1200000
    }
  }
}
```

The client `timeout` must be larger than the server's `timeout_secs`
(900 s default) plus startup headroom, so a slow generation reports as a
readable `isError` from ghostwriter rather than a bare transport timeout
from the client.

On a Linux server, a systemd unit works the same way:
`ExecStart=/usr/local/bin/ghostwriter --config /etc/ghostwriter/config.toml`.

## Development

```sh
cargo test          # unit tests, including retry and contract behavior
cargo clippy --all-targets
cargo fmt --check
```

`src/prompts.rs` holds the style guides — the part that shapes output. Changes
there are behavior changes and deserve a live check against the model, not
just a green test suite.

## License

GPL-2.0-only. The full text is in `LICENSE`.
