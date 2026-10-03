# ghostwriter

ghostwriter is a single-binary MCP server. It produces written documentation using a locally served writing model. Six tools make up the whole surface: `document_code`, `rewrite_prose`, `critique_prose`, `compose`, `style_check`, and `model_health`. Every model-backed tool enforces one house style: formal register, Oxford comma, active voice, no filler, no puff words, and no invented facts. The rewrite and review tools remove the AI-writing patterns from Wikipedia's "Signs of AI writing" list. This pattern set drives the blader/humanizer skill. Any model-backed tool can also compose under a registered writing standard: ASD-STE100, the Google developer documentation style guide, the Microsoft Writing Style Guide, or the Diátaxis documentation architecture. Pass the `style` argument to select a standard.

## Why it exists

Coding agents draft prose as a side effect of coding. Their drafts read like LLM output: hedged, promotional, and padded with filler. The prose that reaches a human (READMEs, API references, standups, release notes) should come from a model whose only job is writing. That model works under rules that reject slop. ghostwriter is that model's tool surface. It solves three problems:

1. Style drift. Every draft passes through the same enforced mechanics and banned-vocabulary lists. Output stays consistent regardless of which agent drafted it.
2. Invented specifics. The drafting guides require every documented signature, flag, or default to be supported by the provided source. `compose` admits gaps instead of guessing at them.
3. Data locality. Requests go to a writing model on your own network. Source code and prose never leave it.

## The tools

| Tool | Input | Output |
| --- | --- | --- |
| `document_code` | Source code (inline or `path`), a document kind, optional audience and notes | A README, API reference, overview, CLI or config doc, changelog entry, or doc-comment |
| `rewrite_prose` | Prose (inline or `path`), optional goal, audience, `voice_sample`, and `style` | The same text with AI-writing patterns, filler, puff words, softeners, and passive voice removed. Facts, code blocks, commands, and paths stay. With `voice_sample`, the rewrite matches the writer's register and word choice. Em-dash rate is best-effort; `critique_prose` judges it against the sample. With `style`, the tool also composes under the selected registered standard |
| `critique_prose` | Prose (inline or `path`), optional reference `source`, `voice_sample`, and `style` | A numbered fault list (AI-writing patterns plus house faults, plus the selected standard's faults when `style` resolves to one), or exactly `CLEAN` |
| `compose` | Raw material (inline or `path`), a kind, optional date, author, length, sections, previous, and `style` | A standup, PRD, one-pager, announcement, summary, release notes, postmortem, weekly status, or meeting notes. `style` composes it under the selected registered standard |
| `style_check` | Prose (inline or `path`), optional `style` (default `ste`) | Deterministic findings with no model call: JSON `{clean, style, findings:[{line, quote, rule, fix}]}`; the `ste` report adds `dictionary`, the shipped dictionary's entry count. `ste`: unapproved general vocabulary with its approved replacement (embedded STE100 Issue 8 dictionary), semicolons, contractions, Latin abbreviations, and sentences over the STE caps (20 words for an instruction, 25 for description). `google`: Latin abbreviations, exclamation marks, tone words (`please`, `simply`, `easily`, `just`, `obviously`, `of course`, `let's`), and internet slang (`tl;dr`, `ymmv`, `rtfm`). `microsoft`: Latin abbreviations, exclamation marks, `please`, and the bias-free term list. Code blocks, inline code, commands, paths, and URLs are exempt. Findings are mechanical: an empty list does not judge style |
| `model_health` | Nothing | Whether the writing-model server is reachable and serving the configured model |

Tool failures travel as `isError` results that the calling agent can read. They never appear as protocol errors. Truncated output carries a visible marker. A half document is never mistaken for a whole one.

## Writing standards (styles)

The registered standards live in one table: `src/styles.rs`. Pass `style: "ste" | "google" | "microsoft" | "diataxis"` to `document_code`, `rewrite_prose`, `critique_prose`, or `compose`. The system prompt gains that standard's rules on top of the house style. `style: "none"` keeps the house style alone. The config file's `style` key is the default for calls that omit the argument.

- `ste`: ASD-STE100 Simplified Technical English. Uses approved general vocabulary from the controlled dictionary, command-form instructions, one idea per sentence, no semicolons, no contractions, no Latin abbreviations, and approved tenses. The rule text summarizes the specification. The dictionary carries the word-level replacement data (word, part of speech, approved replacement). The ASD manual itself is not duplicated in this repository.
- `google`: Google developer documentation style guide. Uses present tense, second person, no `please` and no tone words (`simply`, `easily`) in instructions, serial comma, sentence-case headings, and Latin and internet abbreviations spelled out. This is a paraphrased summary, not an excerpt.
- `microsoft`: Microsoft Writing Style Guide. Uses a simple, direct, friendly voice; encourages contractions; uses serial comma; applies the bias-free term list; and avoids `please` in instructions or UI text. This is a paraphrased summary, not an excerpt.
- `diataxis`: The Diátaxis documentation architecture. Pick the one mode the material fits (tutorial, how-to, reference, or explanation) and write only in it.

`style_check` (default `style: "ste"`) runs the mechanical half of `ste`, `google`, and `microsoft` without a model call. It performs dictionary and word-table lookups plus the line-level rules. An agent can verify a document before or after the model pass. The system refuses a style without mechanical rules with a clear message. An empty findings list never looks like a fake pass.

To make a standard the deployment default, set `style = "ste"` (or any registered id) in the config file, or set `GHOSTWRITER_STYLE=ste` in the environment. Calls that omit the `style` argument then compose under it. A caller can still opt out per call with `style: "none"`. The system refuses the retired `ste = true` config key, the `ste: true` tool argument, and `GHOSTWRITER_STE` at startup or per call. It names the replacement and never silently ignores them.

Adding a standard takes four steps. The module doc of `src/styles.rs` lists them:
1. Paraphrase its rules into a guide in `src/prompts.rs`. Never use an excerpt; the source guides are copyrighted.
2. Declare its mechanical rules, or declare none.
3. Add one row to the registry.
4. Keep `cargo test` green.
Then live-check the guide against the model before shipping it.

## Building

Rust (stable) is the only requirement. The result is one static binary for macOS and Linux.

```sh
cargo build --release
```

The binary is `target/release/ghostwriter`.

## Configuration

Configuration resolves once at startup. It uses layers from lowest to highest priority:

1. Built-in defaults.
2. The config file: `~/.config/ghostwriter/config.toml` (the same path on macOS and Linux) or the file named by `--config <path>`.
3. Environment variables: `GHOSTWRITER_BASE_URL`, `GHOSTWRITER_MODEL`, `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`, `GHOSTWRITER_IDLE_TIMEOUT_SECS`, `GHOSTWRITER_STYLE`.

A missing default config file is fine. The defaults stand. A `--config` path that does not exist is a startup error. Unknown keys in the config file are a startup error. A typo never silently reverts a setting to its default.

```toml
# ~/.config/ghostwriter/config.toml
base_url = "http://hyper03:8002/v1"
model = "hemmingway-1"
temperature = 0.3
timeout_secs = 900
idle_timeout_secs = 60
style = "none"
```

`timeout_secs` bounds one whole generation attempt, including queueing. A busy engine holds a queued request in silence until its first token. `idle_timeout_secs` stops a stream only after it began emitting and then went silent. The server never resends a timed-out or stalled request. It is already working on it. Resends cover only connection failures, 429, 5xx, and empty completions. The MCP client's per-server timeout must exceed `timeout_secs`. Otherwise, the client reports a transport timeout before the server can return a readable error.

The built-in defaults point at the fleet's writing-model server (`http://hyper03:8002/v1`, model `hemmingway-1`). An install anywhere else must set `base_url` and `model` in the file or the environment.

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

The client `timeout` must be larger than the server's `timeout_secs` (900 s default) plus startup headroom. A slow generation reports as a readable `isError` from ghostwriter. It does not report as a bare transport timeout from the client.

On a Linux server, a systemd unit works the same way:
`ExecStart=/usr/local/bin/ghostwriter --config /etc/ghostwriter/config.toml`.

## Development

```sh
cargo test          # unit tests, including retry and contract behavior
cargo clippy --all-targets
cargo fmt --check
```

`src/prompts.rs` holds the style guides. This part shapes output. Changes there are behavior changes. They deserve a live check against the model, not only a green test suite.

## License

GPL-2.0-only. The full text is in `LICENSE`.
