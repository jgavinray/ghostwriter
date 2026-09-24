# ghostwriter

A single-binary MCP server that produces written documentation through a locally
served writing model. Five tools — `document_code`, `rewrite_prose`,
`critique_prose`, `compose`, and `model_health` — carry the whole surface, and
every one of them enforces one house style: formal register, Oxford comma,
active voice, no filler, no puff words, no invented facts — and the rewrite
and review surfaces remove the AI-writing patterns from Wikipedia's
"Signs of AI writing" list (the pattern set behind the blader/humanizer skill).

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
| `rewrite_prose` | Prose (inline or `path`), optional goal, audience, and `voice_sample` | The same text with AI-writing patterns, filler, puff words, softeners, and passive voice removed; facts, code blocks, commands, and paths preserved. With `voice_sample`, the rewrite matches the writer's register and word choice (em-dash rate is best-effort; `critique_prose` judges it against the sample) |
| `critique_prose` | Prose (inline or `path`), optional reference `source` and `voice_sample` | A numbered fault list (AI-writing patterns plus house faults), or exactly `CLEAN` |
| `compose` | Raw material (inline or `path`), a kind, optional date, author, length, sections, previous | A standup, PRD, one-pager, announcement, summary, release notes, postmortem, weekly status, or meeting notes |
| `model_health` | Nothing | Whether the writing-model server is reachable and serving the configured model |

Tool failures travel as `isError` results the calling agent can read, never as
protocol errors. Truncated output carries a visible marker so half a document
is never mistaken for a whole one.

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
   `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`.

A missing default config file is fine; the defaults stand. A `--config` path
that does not exist is a startup error. Unknown keys in the config file are a
startup error, so a typo never silently reverts a setting to its default.

```toml
# ~/.config/ghostwriter/config.toml
base_url = "http://hyper03:8002/v1"
model = "hemmingway-1"
temperature = 0.3
timeout_secs = 300
```

The built-in defaults point at the fleet's writing-model server
(`http://hyper03:8002/v1`, model `hemmingway-1`); an install anywhere else
must set `base_url` and `model`, in the file or the environment.

Verify resolution without starting the server:

```sh
target/release/ghostwriter --self-check
```

## Registering with an MCP client

Point the client at the binary. No environment block is required once the
config file exists:

```json
{
  "mcpServers": {
    "ghostwriter": {
      "command": "/usr/local/bin/ghostwriter",
      "timeout": 600000
    }
  }
}
```

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
