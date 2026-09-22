# AGENTS.md — ghostwriter

## What this is

A single-binary MCP server (rmcp 3.4, tokio, reqwest) exposing five writing
tools over stdio, backed by the locally served `hemmingway-1` model. Coding
agents call it to finalize human-facing prose. The repository lives at
`git@github.com:jgavinray/ghostwriter.git` (remote `origin`, branch `main`);
cross-session state lives in `~/exomemory/`, never here.

## Layout

- `src/main.rs` — entry point. Arg parsing (`--config <path>`, `--self-check`,
  `--help`), one-time config resolution, stdio serving. Exits 1 on any
  configuration that cannot resolve.
- `src/config.rs` — layered config: defaults < `~/.config/ghostwriter/config.toml`
  (same path on macOS and Linux) < `GHOSTWRITER_*` environment variables.
  `build()` is the single validation point; `FileConfig` refuses unknown keys.
- `src/client.rs` — OpenAI-compatible chat client. `AttemptError` classifies
  failures: transport/timeout/429/5xx/empty are retried once, everything else
  (other 4xx, unparseable 2xx) fails immediately.
- `src/writer.rs` — the MCP surface. Five `#[tool]` handlers, argument structs,
  truncation markers, `error_result`.
- `src/prompts.rs` — the style guides. This is the product: the guides are why
  output is not generic LLM prose. Treat edits here as behavior changes.

## Invariants (do not break)

- Tool-level failures (bad arguments, unreadable files, model errors) travel as
  `CallToolResult::error` — `isError: true` content the calling agent can
  read. Never a JSON-RPC protocol error. The tests in `writer.rs` pin this.
- Truncation is always visible: a capped disk read appends a truncation marker
  (`MAX_SOURCE_BYTES`), and a `finish_reason: "length"` answer appends a
  re-call hint. Silent truncation is a bug.
- Unknown `kind` values are refused with the valid list — never guessed at.
- Inline payloads are fenced with four backticks in `user_prompt`; shorter
  fences collide with real content.
- `MAX_TOKENS_CAP` (32768) exists because the served context is 131072 tokens;
  do not raise it without checking the model's context.
- Retry policy: exactly one resend, only for `AttemptError::Retryable`. The
  tests in `client.rs` assert actual connection counts against a local HTTP
  server.
- "hemmingway" (two m's, single h) is the served model id — intentional, do
  not "fix" the spelling.

## Config contract

- File: `~/.config/ghostwriter/config.toml`, universal across macOS and Linux
  per owner decision (no XDG redirection, no Library path).
- `--config` names a file that must exist; the default path may be absent.
- Env vars: `GHOSTWRITER_BASE_URL`, `GHOSTWRITER_MODEL`,
  `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`. There are no legacy
  `HEMMINGWAY_*` aliases.
- Defaults point at the fleet box (`http://hyper03:8002/v1`); installs
  elsewhere must override `base_url` and `model`.

## Verify before claiming done

```sh
cargo test                    # 16 tests, all must pass
cargo clippy --all-targets    # clean, no warnings
cargo fmt --check             # clean
cargo build --release         # the deployed artifact
target/release/ghostwriter --self-check
```

The release binary is what the MCP client runs (see the `ghostwriter` entry in
`~/.omp/agent/mcp.json`). After any source change, rebuild it and re-run
`--self-check`; a source-only change ships nothing.

## Live checks

Prompt-guide changes need a live call, not just tests: pipe an initialize +
`tools/call` handshake into the binary over stdio, or invoke the registered
tool from an omp session (`model_health` is the cheapest probe). The style
guides are judged by their effect on model output; a green test suite says
nothing about prose quality.

## Deployment notes

- The MCP client registration carries no `env` block; the config file at
  `~/.config/ghostwriter/config.toml` is the deployment surface. Env vars
  remain available for per-launch overrides.
- omp sessions started before an `mcp.json` or binary change keep the old
  server until the next omp start.
