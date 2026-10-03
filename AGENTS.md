# AGENTS.md — ghostwriter

## What this is

A single-binary MCP server (rmcp 3.4, tokio, reqwest) exposing six writing
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
- `src/client.rs` — OpenAI-compatible chat client; completions stream as
  SSE (`stream: true`). `SseTail` accumulates deltas and refuses a stream
  that ends without `[DONE]`. `AttemptError` classifies failures: connection
  faults before any response, 429, 5xx, and empty completions are retried
  once; timeouts, idle stalls (silence past `idle_timeout` once the stream
  has emitted its first token), other 4xx, broken chunk JSON, and
  mid-generation stream death fail immediately — resending work the server
  already accepted only doubles the load.
- `src/writer.rs` — the MCP surface. Six `#[tool]` handlers, argument structs,
  truncation markers, `error_result`.
- `src/prompts.rs` — the style guides. This is the product: the guides are why
  output is not generic LLM prose. Treat edits here as behavior changes.
- `src/styles.rs` — the style registry, the extension point for external
  writing standards. One row per standard binds an id (`style: "ste"` etc.)
  to its prompt guide (from `prompts`), its head line, its critique judging
  sentence, and an optional mechanical rule set (`Rules`) for
  `style_check`. Adding a standard is the four-step recipe in the module
  doc; registered today: `ste`, `google`, `microsoft`, `diataxis` (Diátaxis
  is a structure framework, not a prose style, so it carries no mechanical
  rules and `style_check` refuses it with a clear message).
- `src/ste.rs` — the deterministic mechanical engine behind `style_check`:
  dictionary lookups and line-level rules run through `check_with()` under
  a `Rules` policy (which primitives fire, which extra word table applies).
  `STE_RULES` is the ASD-STE100 set; `ste::check()` is its pinned wrapper.
  The prompt-side rule summaries live in `prompts`, the style bindings in
  `styles`. The approved-general-vocabulary dictionary ships as
  `assets/ste100-unapproved.tsv` (`word<TAB>pos<TAB>APPROVED REPLACEMENT`,
  `#` comments), embedded with `include_str!` at compile time. It is
  byte-reproducible: `pdftotext -layout` the ASD-STE100 Issue 8 PDF, then
  `python3 assets/parse.py <pdftotext-output.txt>
  assets/ste100-unapproved.tsv` (provenance note in the script docstring).

## Invariants (do not break)

- Tool-level failures (bad arguments, unreadable files, model errors) travel as
  `CallToolResult::error` — `isError: true` content the calling agent can
  read. Never a JSON-RPC protocol error. The tests in `writer.rs` pin this.
- Truncation is always visible: a capped disk read appends a truncation marker
  (`MAX_SOURCE_BYTES`), and a `finish_reason: "length"` answer appends a
  re-call hint. Silent truncation is a bug.
- Unknown `kind` values are refused with the valid list — never guessed at.
  Unknown `style` values are refused the same way. The retired `ste: true`
  tool argument, `ste = true` config key, and `GHOSTWRITER_STE` env var are
  refused loudly with `style` named as the replacement — a stale caller
  must never silently lose the standard it asked for.
- Inline payloads are fenced with four backticks in `user_prompt`; shorter
  fences collide with real content.
- `MAX_TOKENS_CAP` (32768) exists because the served context is 131072 tokens;
  do not raise it without checking the model's context.
- Retry policy: exactly one resend, only for `AttemptError::Retryable`. The
  tests in `client.rs` assert actual connection counts against a local HTTP
  server.
- "hemmingway" (two m's, single h) is the served model id — intentional, do
  not "fix" the spelling.
- `style_check` performs no network call: its result is a pure function of
  the text and the registered `Rules`, so tests can pin exact findings. A
  style without mechanical rules must be refused, never report `clean`.
  The dictionary is word-level replacement data (word, part of speech,
  approved replacement); the ASD manual's own text does not enter the
  repository — `STE_GUIDE` is a summary, not an excerpt. The same holds for
  every registered guide: source guides are copyrighted, so `prompts.rs`
  carries paraphrases only.

## Config contract

- File: `~/.config/ghostwriter/config.toml`, universal across macOS and Linux
  per owner decision (no XDG redirection, no Library path).
- `--config` names a file that must exist; the default path may be absent.
- Env vars: `GHOSTWRITER_BASE_URL`, `GHOSTWRITER_MODEL`,
  `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`,
  `GHOSTWRITER_IDLE_TIMEOUT_SECS`, `GHOSTWRITER_STYLE` (a registered id,
  case-insensitive). `GHOSTWRITER_STE` is retired and refused at startup.
  There are no legacy `HEMMINGWAY_*` aliases.
- `style` (string, default `"none"`) is the server-side default for the
  per-call `style` argument on the four model tools, validated against the
  registry at startup: with `style = "ste"`, calls that omit the argument
  compose in ASD-STE100; an explicit `style` from the caller — including
  `"none"` — always wins. `style_check` deliberately ignores this key and
  always defaults to `ste`: its report shape is a deterministic contract
  (pinned tests), independent of the deployment's writing default.
- Defaults point at the fleet box (`http://hyper03:8002/v1`); installs
  elsewhere must override `base_url` and `model`.

## Verify before claiming done

```sh
cargo test                    # 63 tests, all must pass
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
