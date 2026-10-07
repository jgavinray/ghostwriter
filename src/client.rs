//! OpenAI-compatible chat client for the hemmingway-1 vLLM server, on
//! reqwest. Completions stream as SSE so a healthy-but-slow generation is
//! never mistaken for a dead one: the idle timeout kills a stream that went
//! silent after its first token, and the overall timeout bounds a runaway
//! one and the queue ahead of it. Neither is resent — the server already
//! accepted the request and is spending compute
//! on it, so a resend only doubles the load and guarantees the caller's
//! outer budget expires before either attempt reports. Resends cover the
//! faults a resend can actually heal: connection failures before any
//! response byte, HTTP 429, HTTP 5xx, and empty completions. Everything
//! else — any other 4xx, broken chunk JSON, a stream that dies after its
//! first byte — fails immediately with the server's own explanation (a bad
//! model id will not heal on resend).

use serde_json::{json, Value};
use std::time::Duration;

use crate::config::Config;

#[derive(Debug)]
pub struct Completion {
    pub text: String,
    /// vLLM `finish_reason: "length"` — the answer was cut at max_tokens.
    pub truncated: bool,
}

/// One failed attempt, classified by whether resending can plausibly help.
#[derive(Debug)]
enum AttemptError {
    /// Connection failure before any response byte, HTTP 429, HTTP 5xx — a
    /// resend is justified because the server did not start the work.
    Retryable(String),
    /// Timeout or stall (the server is working on this request right now),
    /// any other 4xx, broken stream mid-generation, or an unparseable
    /// body — resending cannot heal it, only stack load.
    Fatal(String),
}

pub struct Client {
    http: reqwest::Client,
    /// Short-budget client for health reporting: a model list that takes
    /// minutes is the same outage the completion call is about to hit, and
    /// `model_health` must answer in seconds.
    http_health: reqwest::Client,
}

/// The full reqwest cause chain. `Display` for a request error names
/// only the kind ("error sending request for url (...)"); the detail
/// that distinguishes a real TLS handshake failure ("tls handshake
/// eof", tokio-rustls) from a no-TLS-backend build that never attempts
/// one ("invalid URL, scheme is not http", hyper-util's plain
/// connector) lives in the sources. Surfacing them makes the failure
/// reportable to the caller — and is what lets the HTTPS pin below
/// discriminate TLS.
fn err_chain(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(": ")
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Client, String> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("building HTTP client failed: {e}"))?;
        let http_health = reqwest::Client::builder()
            .timeout(cfg.idle_timeout)
            .build()
            .map_err(|e| format!("building health HTTP client failed: {e}"))?;
        Ok(Client { http, http_health })
    }

    pub async fn complete(
        &self,
        cfg: &Config,
        system: &str,
        user: &str,
        max_tokens: u32,
        temperature: f32,
    ) -> Result<Completion, String> {
        let mut messages = vec![json!({"role": "user", "content": user})];
        if !system.is_empty() {
            messages.insert(0, json!({"role": "system", "content": system}));
        }
        let body = json!({
            "model": cfg.model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": true,
            // vLLM-specific: served models with a reasoning parser (qwen38)
            // otherwise bill thinking tokens against max_tokens, which can
            // leave message content null and surface as "empty completion".
            // Other vLLM templates ignore an unknown chat_template kwarg.
            "chat_template_kwargs": {"enable_thinking": false},
        });

        let mut last_err = String::new();
        for attempt in 0..2 {
            match self.try_once(cfg, &body).await {
                Ok(c) if c.text.is_empty() => last_err = "empty completion from server".to_string(),
                Ok(c) => return Ok(c),
                Err(AttemptError::Retryable(e)) => last_err = e,
                Err(AttemptError::Fatal(e)) => {
                    return Err(format!("{} request failed: {e}", cfg.model))
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        Err(format!(
            "{} request failed after 2 attempts: {last_err}",
            cfg.model
        ))
    }

    async fn try_once(&self, cfg: &Config, body: &Value) -> Result<Completion, AttemptError> {
        let url = format!("{}/chat/completions", cfg.base_url);
        let resp = self.http.post(&url).json(body).send().await.map_err(|e| {
            let detail = format!("request to {url} failed: {}", err_chain(&e));
            // The overall timeout expiring before headers means the
            // engine is busy with this very request; a resend queues
            // the same work a second time.
            if e.is_timeout() {
                AttemptError::Fatal(detail)
            } else {
                AttemptError::Retryable(detail)
            }
        })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            // 429/5xx may heal on resend; any other 4xx will not. Surface
            // the server's own explanation either way.
            // Bound the read: a broken server must not make us buffer an
            // unbounded error body — clip() truncates to far less anyway,
            // and the helper stops draining (never just hides) the excess.
            let mut resp = resp;
            let (raw, _) = read_capped(&mut resp)
                .await
                .map_err(AttemptError::Retryable)?;
            let detail = format!(
                "HTTP {status} from {url}: {}",
                clip(&String::from_utf8_lossy(&raw))
            );
            return Err(if status == 429 || status >= 500 {
                AttemptError::Retryable(detail)
            } else {
                AttemptError::Fatal(detail)
            });
        }

        // Stream the SSE body. Before the first token the request is queued
        // inside the engine and silence is normal — only the overall
        // timeout may kill it. Once bytes have started flowing, silence is
        // death and the idle timeout takes over.
        let mut sse = SseTail::default();
        let mut resp = resp;
        loop {
            let next = if sse.bytes_seen {
                tokio::time::timeout(cfg.idle_timeout, resp.chunk())
                    .await
                    .map_err(|_| {
                        AttemptError::Fatal(format!(
                            "stream from {url} stalled: no bytes within {}s ({} chars received)",
                            cfg.idle_timeout.as_secs(),
                            sse.text.len()
                        ))
                    })?
            } else {
                // Bounded overall by reqwest's .timeout(cfg.timeout).
                resp.chunk().await
            }
            .map_err(|e| {
                let detail = format!("stream from {url} failed: {e}");
                // A dropped connection before the first byte is a dead
                // worker a resend can survive; once any byte has arrived
                // the engine has this request, and the overall timeout
                // means it is mid-generation — neither heals on resend.
                if !sse.bytes_seen && !e.is_timeout() {
                    AttemptError::Retryable(detail)
                } else {
                    AttemptError::Fatal(detail)
                }
            })?;
            let Some(bytes) = next else { break };
            sse.bytes_seen = true;
            sse.push(&bytes).map_err(AttemptError::Fatal)?;
            if sse.done {
                break;
            }
        }
        sse.finish(&url)
    }

    /// Served-model list for health reporting.
    pub async fn list_models(&self, cfg: &Config) -> Result<Vec<String>, String> {
        let url = format!("{}/models", cfg.base_url);
        let mut resp = self
            .http_health
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;
        let status = resp.status().as_u16();
        let (raw, overflowed) = read_capped(&mut resp).await?;
        if status != 200 {
            return Err(format!(
                "HTTP {status} from {url}: {}",
                clip(&String::from_utf8_lossy(&raw))
            ));
        }
        if overflowed {
            // The prefix is deliberately not parsed: a /models list past
            // what any honest server sends is deemed invalid, never
            // truncated into a partial truth.
            return Err(format!(
                "{} returned an unreadable /models response",
                cfg.model
            ));
        }
        let text = String::from_utf8_lossy(&raw);
        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("unparseable /models body: {e}"))?;
        Ok(value
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Read a response body with the hard cap applied: at most
/// `MAX_ERROR_BODY_BYTES + 1` bytes ever reach memory, and past the cap the
/// reader stops draining entirely (the socket dies on its own) rather than
/// hiding an unbounded download. The second element reports whether the body
/// exceeded the cap; callers that display the body (clipped to far less)
/// ignore it, callers that would parse it must not.
async fn read_capped(resp: &mut reqwest::Response) -> Result<(Vec<u8>, bool), String> {
    let mut raw: Vec<u8> = Vec::new();
    let mut overflowed = false;
    loop {
        match resp.chunk().await {
            Ok(Some(b)) => {
                let room = (MAX_ERROR_BODY_BYTES + 1).saturating_sub(raw.len());
                raw.extend_from_slice(&b[..b.len().min(room)]);
                if raw.len() > MAX_ERROR_BODY_BYTES {
                    overflowed = true;
                    break; // stop draining; the socket closes on its own
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("reading response body failed: {e}")),
        }
    }
    Ok((raw, overflowed))
}

/// Incremental SSE reader: buffers raw bytes, parses complete `data:` lines
/// as they form, accumulates `choices[0].delta.content`, and remembers
/// `finish_reason: "length"` and the `[DONE]` sentinel.
#[derive(Default)]
struct SseTail {
    pending: Vec<u8>,
    text: String,
    truncated: bool,
    done: bool,
    /// Any raw body byte received. Until then the request is queued in the
    /// engine and the idle timer must not run; queueing is silent by design.
    bytes_seen: bool,
}

impl SseTail {
    /// Feed one raw chunk. Chunk boundaries may split a line anywhere; only
    /// complete lines are parsed. Broken chunk JSON is a broken server, not
    /// a transient fault.
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_SSE_PENDING_BYTES {
            return Err(format!(
                "unbounded SSE line: {} bytes without a newline",
                self.pending.len()
            ));
        }
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=nl).collect();
            self.handle_line(&line)?;
        }
        Ok(())
    }

    fn handle_line(&mut self, line: &[u8]) -> Result<(), String> {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end().trim_start();
        let Some(payload) = line.strip_prefix("data:") else {
            return Ok(()); // comments, event:/id: fields, blank separators
        };
        let payload = payload.trim_start();
        if payload == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(payload)
            .map_err(|e| format!("unparseable SSE chunk: {} ({e})", clip(payload)))?;
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return Ok(());
        };
        if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
            self.truncated = true;
        }
        if let Some(delta) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
        {
            self.text.push_str(delta);
            if self.text.len() > MAX_SSE_TEXT_BYTES {
                return Err(format!(
                    "server sent more text than any completion can hold ({} bytes)",
                    self.text.len()
                ));
            }
        }
        Ok(())
    }

    /// A stream that reached EOF without [DONE] is truncated mid-decode;
    /// shipping the partial text as if it were complete would be the worst
    /// possible failure mode for a writing tool.
    fn finish(self, url: &str) -> Result<Completion, AttemptError> {
        if !self.done {
            return Err(AttemptError::Fatal(format!(
                "stream from {url} ended before [DONE] ({} chars received)",
                self.text.len()
            )));
        }
        Ok(Completion {
            text: self.text,
            truncated: self.truncated,
        })
    }
}

/// Hard caps on what a single exchange may make us buffer: no error body
/// needs more than this (it is clipped far shorter), no SSE line survives
/// 16 MiB of buffer without a newline, and no completion holds 4 MiB of
/// text (MAX_TOKENS_CAP cannot produce it).
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_SSE_PENDING_BYTES: usize = 16 * 1024 * 1024;
const MAX_SSE_TEXT_BYTES: usize = 4 * 1024 * 1024;

fn clip(s: &str) -> String {
    const CAP: usize = 500;
    if s.len() <= CAP {
        s.to_string()
    } else {
        let mut end = CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn sse_chunk(text: &str, finish: Option<&str>) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": text}, "finish_reason": finish}]})
        )
    }

    #[test]
    fn sse_accumulates_deltas_across_chunk_splits() {
        let mut sse = SseTail::default();
        let stream = format!(
            "{}{}{}",
            sse_chunk("Hello ", None),
            sse_chunk("world", Some("stop")),
            "data: [DONE]\n\n"
        );
        // Feed it one byte at a time: line buffering must survive any split.
        for b in stream.as_bytes() {
            sse.push(&[*b]).unwrap();
        }
        let c = sse.finish("test").unwrap();
        assert_eq!(c.text, "Hello world");
        assert!(!c.truncated);
    }

    #[test]
    fn sse_flags_length_truncation() {
        let mut sse = SseTail::default();
        sse.push(sse_chunk("cut off here", Some("length")).as_bytes())
            .unwrap();
        sse.push(b"data: [DONE]\n\n").unwrap();
        let c = sse.finish("test").unwrap();
        assert!(c.truncated);
        assert_eq!(c.text, "cut off here");
    }

    #[test]
    fn sse_rejects_broken_json_and_missing_done() {
        let mut sse = SseTail::default();
        assert!(sse.push(b"data: {not json}\n\n").is_err());

        let mut sse = SseTail::default();
        sse.push(sse_chunk("half a story", None).as_bytes())
            .unwrap();
        // EOF without the sentinel: partial text must not pass as complete.
        assert!(sse.finish("test").is_err());
    }

    /// Local HTTP server handing out canned statuses, counting connections.
    async fn serve(statuses: Vec<u16>) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            let mut statuses = statuses.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let Some(status) = statuses.next() else { break };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reason = match status {
                    429 => "Too Many Requests",
                    400 => "Bad Request",
                    _ => "Internal Server Error",
                };
                let body = format!("{{\"error\":\"status {status}\"}}");
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (url, hits)
    }

    /// Local SSE server: writes each script entry as its own chunk, then
    /// holds the socket open for `linger` (never sending [DONE] unless the
    /// script does), so idle-timeout paths exercise realistically.
    async fn serve_sse(chunks: Vec<String>, linger: Duration) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                for c in &chunks {
                    let _ = sock.write_all(c.as_bytes()).await;
                }
                tokio::time::sleep(linger).await;
            }
        });
        (url, hits)
    }

    /// Server that accepts, reads the request, and never answers.
    async fn serve_silent() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        (url, hits)
    }

    fn http_config(base_url: String) -> Config {
        Config {
            base_url,
            model: "test-model".into(),
            temperature: 0.3,
            timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
            style: "none".into(),
        }
    }

    /// A 4xx other than 429 fails on the first attempt — no resend.
    #[tokio::test]
    async fn fatal_4xx_is_not_retried() {
        let (url, hits) = serve(vec![400]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("HTTP 400"), "unexpected error: {err}");
        assert!(!err.contains("after 2 attempts"), "4xx retried: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// 5xx gets exactly one resend, then the error surfaces.
    #[tokio::test]
    async fn server_error_5xx_is_retried_once() {
        let (url, hits) = serve(vec![500, 500]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("after 2 attempts"), "unexpected error: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stream_completes_end_to_end() {
        let chunks = vec![
            sse_chunk("Rewritten: ", None),
            sse_chunk("the text.", Some("stop")),
            "data: [DONE]\n\n".to_string(),
        ];
        let (url, hits) = serve_sse(chunks, Duration::from_millis(50)).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let c = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap();
        assert_eq!(c.text, "Rewritten: the text.");
        assert!(!c.truncated);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// First delta arrives, then silence: the idle timeout must kill the
    /// attempt and — because the engine was mid-generation — it must NOT
    /// be resent.
    #[tokio::test]
    async fn stalled_stream_is_killed_without_resend() {
        let chunks = vec![sse_chunk("slow ", None)];
        let (url, hits) = serve_sse(chunks, Duration::from_secs(30)).await;
        let mut cfg = http_config(url.clone());
        cfg.idle_timeout = Duration::from_millis(300);
        cfg.timeout = Duration::from_secs(20);
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("stalled"), "unexpected error: {err}");
        assert!(
            err.contains("5 chars received"),
            "lost progress count: {err}"
        );
        assert!(!err.contains("after 2 attempts"), "stall resent: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Headers never arrive (engine wedged before scheduling): the overall
    /// timeout fires and the request is NOT resent into the same wedged queue.
    #[tokio::test]
    async fn overall_timeout_is_not_resent() {
        let (url, hits) = serve_silent().await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("request to"), "unexpected error: {err}");
        assert!(!err.contains("after 2 attempts"), "timeout resent: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Headers arrive but the engine keeps the request queued in silence
    /// (busy batching another generation): the idle timer must NOT fire
    /// pre-first-token; the overall timeout decides, and it is not resent.
    #[tokio::test]
    async fn queued_silence_waits_the_overall_timeout_not_the_idle_kill() {
        let (url, hits) = serve_sse(vec![], Duration::from_secs(30)).await;
        let mut cfg = http_config(url.clone());
        cfg.idle_timeout = Duration::from_millis(200);
        cfg.timeout = Duration::from_secs(2);
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(
            !err.contains("stalled"),
            "idle kill fired on a queued request: {err}"
        );
        assert!(!err.contains("after 2 attempts"), "queue resent: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// SSE server that announces a body far larger than it writes: the
    /// client sees the 200 head plus one comment line, then the body is cut
    /// short when the socket closes — a mid-stream drop with bytes seen but
    /// zero content text.
    async fn serve_sse_truncated_early(comment: String) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body_len = comment.len() + 1024;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {body_len}\r\n\r\n{comment}"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                // Body short by 1024 bytes: drop the socket and a later
                // chunk() must surface a mid-body error.
                drop(sock);
            }
        });
        (url, hits)
    }

    /// Headers + keep-alive bytes arrived — the engine already has this
    /// request — then the stream died before any content: the documented
    /// policy is fail immediately; a resend only doubles the load.
    #[tokio::test]
    async fn stream_drop_after_bytes_is_not_resent() {
        let (url, hits) = serve_sse_truncated_early(": keepalive\n\n".to_string()).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(
            !err.contains("after 2 attempts"),
            "mid-stream drop after bytes was resent: {err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A 429 may heal on resend: exactly one resend, then the error surfaces.
    #[tokio::test]
    async fn http_429_is_retried_once() {
        let (url, hits) = serve(vec![429, 429]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("HTTP 429"), "unexpected error: {err}");
        assert!(err.contains("after 2 attempts"), "429 not retried: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A well-formed stream that yields zero content is resent exactly once.
    #[tokio::test]
    async fn empty_completion_is_retried_once() {
        let chunks = vec![sse_chunk("", Some("stop")), "data: [DONE]\n\n".to_string()];
        let (url, hits) = serve_sse(chunks, Duration::from_millis(50)).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("empty completion"), "unexpected error: {err}");
        assert!(
            err.contains("after 2 attempts"),
            "empty completion not retried: {err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// Nothing ever listens: a connection failure before any response is
    /// resent exactly once.
    #[tokio::test]
    async fn refused_connection_resends_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let cfg = http_config(format!("http://{addr}"));
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(
            err.contains("after 2 attempts"),
            "refused connection not resent: {err}"
        );
    }

    /// The HTTPS pin, with a discriminator that actually discriminates
    /// TLS. Two independent guards, both mutation-verified (2026-10-07)
    /// against a /tmp copy with `"rustls"` removed from the reqwest
    /// feature list in Cargo.toml:
    ///
    /// 1. Compile-time: the builder call below names
    ///    `ClientBuilder::tls_backend_rustls`, which reqwest 0.13.5
    ///    exposes only under its `rustls` feature (`__rustls` cfg).
    ///    Drop the feature and this test stops compiling — loudly, at
    ///    the type check, not as a passing assertion.
    /// 2. Runtime: a real TLS attempt against a plain-TCP endpoint that
    ///    accepts the connection, lets rustls send its ClientHello, then
    ///    hangs up mid-handshake, observed to fail with
    ///    "tls handshake eof". A no-TLS-backend build never reaches a
    ///    handshake at all: reqwest's connector is the plain HTTP one
    ///    and hyper-util refuses the `https` scheme with "invalid URL,
    ///    scheme is not http" (both texts observed on this host, rustls
    ///    present vs removed). The endpoint is a listener that stays
    ///    open, so both resent attempts reach it and the connection-
    ///    fault shape ("after 2 attempts") still holds.
    #[tokio::test]
    async fn https_base_url_attempts_tls_instead_of_refusing_to_build() {
        // rustls-gated: `tls_backend_rustls` does not exist on the
        // builder without the reqwest `rustls` feature. This call IS
        // the compile-time half of the pin — deleting the feature turns
        // this test into a build failure, so it can never pass while
        // quietly not being a TLS build.
        let _ = reqwest::Client::builder().tls_backend_rustls();

        let (port, stop) = spawn_fake_tls_endpoint().await;
        let cfg = http_config(format!("https://127.0.0.1:{port}/v1"));
        let client = Client::new(&cfg)
            .expect("client with an https base_url must build once rustls is wired in");
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        stop.store(true, Ordering::SeqCst);

        // The Retryable connection-fault shape of `try_once`: the send
        // died before any response byte, so both attempts ran and the
        // message names the url.
        assert!(
            err.contains("request to https://127.0.0.1:")
                && err.contains("/v1/chat/completions failed")
                && err.contains("after 2 attempts"),
            "expected the connection-fault path of a real TLS attempt: {err}"
        );
        // The TLS-only half: this text comes from the rustls handshake
        // and cannot appear without a TLS backend.
        assert!(
            err.contains("tls handshake eof"),
            "no TLS handshake was ever attempted: {err}"
        );
        // The no-TLS-build marker, observed byte-for-byte when the
        // feature is removed: its presence means we are back to the
        // plain connector refusing the https scheme before any wire
        // traffic.
        assert!(
            !err.contains("invalid URL, scheme is not http"),
            "the no-TLS-backend build path is back: {err}"
        );
    }

    /// A listener that accepts, drains the client's ClientHello, then
    /// hangs up without ever answering the handshake. It is not a TLS
    /// server — that is the point: a real TLS client dies mid-handshake
    /// against it ("tls handshake eof"), while a no-TLS-build connector
    /// refuses the url before writing anything. The drain matters: close
    /// with the ClientHello still unread in the socket makes the kernel
    /// send RST, and the client would report a reset instead of the
    /// handshake EOF. Kept accepting until `stop` flips so the resent
    /// attempt hits the same behaviour. Returns the port and the stop
    /// flag.
    async fn spawn_fake_tls_endpoint() -> (u16, Arc<AtomicBool>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        tokio::spawn(async move {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept().await {
                    Ok((mut sock, _)) => {
                        tokio::spawn(async move {
                            let mut buf = [0u8; 4096];
                            // Read until quiet: the client writes its
                            // whole ClientHello in one go, then waits
                            // for a ServerHello that never comes.
                            loop {
                                match tokio::time::timeout(
                                    Duration::from_millis(150),
                                    sock.read(&mut buf),
                                )
                                .await
                                {
                                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                    Ok(Ok(_)) => {}
                                }
                            }
                            // FIN, not RST: the reader saw everything.
                            let _ = sock.shutdown().await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    /// The SseTail caps are enforced directly, without a server: a line
    /// that never terminates must not buffer without bound…
    #[test]
    fn sse_rejects_unbounded_pending_line() {
        let mut sse = SseTail::default();
        let huge = vec![b'a'; 16 * 1024 * 1024 + 1];
        let err = sse.push(&huge).unwrap_err();
        assert!(
            err.contains("unbounded SSE line"),
            "unexpected error: {err}"
        );
    }

    /// …and accumulated text beyond any conceivable completion must abort
    /// the stream instead of accumulating forever.
    #[test]
    fn sse_rejects_unbounded_text() {
        let mut sse = SseTail::default();
        let piece = "x".repeat(4096);
        let delta = json!({"choices": [{"delta": {"content": piece}}]}).to_string();
        let line = format!("data: {delta}\n\n");
        for _ in 0..(4 * 1024 * 1024 / 4096) {
            sse.push(line.as_bytes()).unwrap();
        }
        let err = sse.push(line.as_bytes()).unwrap_err();
        assert!(
            err.contains("more text than any completion can hold"),
            "unexpected error: {err}"
        );
    }

    /// User-facing errors name the configured model, not a hardcoded id.
    #[tokio::test]
    async fn errors_name_the_configured_model() {
        let (url, _hits) = serve(vec![400]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(
            err.contains("test-model request failed"),
            "unexpected error: {err}"
        );

        let (url, _hits) = serve(vec![500, 500]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(
            err.contains("test-model request failed after 2 attempts"),
            "unexpected error: {err}"
        );
    }

    // --- m8: the 64 KiB error-body cap must actually stop draining ---------

    /// Server that answers with a 400 whose body is `body_len` bytes of
    /// junk, and reports — server-side, where the client cannot fake it —
    /// how many bytes the client actually pulled, and whether the write
    /// ever completed. `content-length` matches the body exactly, so a
    /// client that fully drains leaves the server with a completed write;
    /// a client that stops reading hangs the server up with EPIPE.
    async fn serve_huge_error(body_len: usize) -> (String, Arc<AtomicU64>, Arc<AtomicBool>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let pulled = Arc::new(AtomicU64::new(0));
        let counted = pulled.clone();
        let drained = Arc::new(AtomicBool::new(false));
        let drained_flag = drained.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {body_len}\r\n\r\n"
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    continue;
                }
                let piece = vec![b'x'; 8 * 1024];
                let mut left = body_len;
                let mut ok = true;
                while left > 0 {
                    let n = left.min(piece.len());
                    if sock.write_all(&piece[..n]).await.is_err() {
                        ok = false; // the client stopped reading and hung up
                        break;
                    }
                    counted.fetch_add(n as u64, Ordering::SeqCst);
                    left -= n;
                }
                drained_flag.store(ok, Ordering::SeqCst);
            }
        });
        (url, pulled, drained)
    }

    /// The 64 KiB error-body cap must bound the wire, not just our buffer:
    /// a client that stops draining at the cap leaves the server's writes
    /// unfinished (EPIPE once the hangup reaches it). An unbounded
    /// `resp.text()` drains the entire body and the server observes a
    /// complete write — pre-fix, both assertions below fail. The body is
    /// 32 MiB so no socket-buffer capacity can absorb it invisibly.
    #[tokio::test]
    async fn error_body_read_stops_at_the_cap() {
        const BODY: usize = 32 * 1024 * 1024;
        let (url, pulled, drained) = serve_huge_error(BODY).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.complete(&cfg, "s", "u", 16, 0.3).await.unwrap_err();
        assert!(err.contains("HTTP 400"), "unexpected error: {err}");
        assert!(!err.contains("after 2 attempts"), "4xx resent: {err}");
        // Let the server observe the client's hangup (the reset arrives
        // after the response object drops) before reading the counters.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let seen = pulled.load(Ordering::SeqCst);
        assert!(
            !drained.load(Ordering::SeqCst),
            "the client drained the whole {BODY}-byte error body ({seen} bytes); \
             the 64 KiB cap did not stop the drain"
        );
        assert!(
            seen < BODY as u64 / 8,
            "client pulled {seen} bytes of error body — far past the cap \
             plus any plausible kernel buffering"
        );
    }

    // --- m8: /models is bounded the same way ---------------------------------

    /// Canned /models server: serves each scripted 200 body to one
    /// connection, counting connections.
    async fn serve_models(bodies: Vec<String>) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            let mut bodies = bodies.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let Some(body) = bodies.next() else { break };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (url, hits)
    }

    /// A /models response larger than the cap is refused as unreadable —
    /// the truncated prefix is never parsed — while an honest small body
    /// still parses exactly as before. Pre-fix the unbounded `resp.text()`
    /// happily parses this very body, so the unwrap_err fails.
    #[tokio::test]
    async fn list_models_refuses_an_oversized_body() {
        // Valid JSON end to end: any reader that buffers it all parses it.
        let fat = json!({
            "object": "list",
            "data": [{ "id": "served-a", "padding": "x".repeat(1024 * 1024) }],
        })
        .to_string();
        assert!(fat.len() > 64 * 1024);
        let (url, hits) = serve_models(vec![fat]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        let err = client.list_models(&cfg).await.unwrap_err();
        assert!(
            err.contains("test-model returned an unreadable /models response"),
            "oversized /models body accepted: {err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "health call was resent");

        let honest = json!({
            "object": "list",
            "data": [{ "id": "served-a" }, { "id": "served-b" }],
        })
        .to_string();
        let (url, _hits) = serve_models(vec![honest]).await;
        let cfg = http_config(url.clone());
        let client = Client::new(&cfg).unwrap();
        assert_eq!(
            client.list_models(&cfg).await.unwrap(),
            vec!["served-a".to_string(), "served-b".to_string()]
        );
    }
}
