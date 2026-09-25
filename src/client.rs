//! OpenAI-compatible chat client for the hemmingway-1 vLLM server, on
//! reqwest. Completions stream as SSE so a healthy-but-slow generation is
//! never mistaken for a dead one: the idle timeout kills a stream that went
//! silent after its first token, and the overall timeout bounds a runaway
//! one and the queue ahead of it. Neither is resent — the server already
//! accepted the request and is spending compute
//! on it, so a resend only doubles the load and guarantees the caller's
//! outer budget expires before either attempt reports. Resends cover the
//! faults a resend can actually heal: connection failures before any
//! response, HTTP 429, HTTP 5xx, and empty completions. Everything else —
//! any other 4xx, broken chunk JSON, a stream that dies before [DONE] —
//! fails immediately with the server's own explanation (a bad model id will
//! not heal on resend).

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
    /// Connection failure before any response, HTTP 429, HTTP 5xx — a
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
        });

        let mut last_err = String::new();
        for attempt in 0..2 {
            match self.try_once(cfg, &body).await {
                Ok(c) if c.text.is_empty() => last_err = "empty completion from server".to_string(),
                Ok(c) => return Ok(c),
                Err(AttemptError::Retryable(e)) => last_err = e,
                Err(AttemptError::Fatal(e)) => {
                    return Err(format!("hemmingway-1 request failed: {e}"))
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        Err(format!(
            "hemmingway-1 request failed after 2 attempts: {last_err}"
        ))
    }

    async fn try_once(&self, cfg: &Config, body: &Value) -> Result<Completion, AttemptError> {
        let url = format!("{}/chat/completions", cfg.base_url);
        let resp = self.http.post(&url).json(body).send().await.map_err(|e| {
            let detail = format!("request to {url} failed: {e}");
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
            let text = resp.text().await.map_err(|e| {
                AttemptError::Retryable(format!("reading response body failed: {e}"))
            })?;
            let detail = format!("HTTP {status} from {url}: {}", clip(&text));
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
                // A dropped connection before the first token is a
                // dead worker a resend can survive; anything after text
                // began streaming, or the overall timeout, is not.
                if !e.is_timeout() && sse.text.is_empty() {
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
        let resp = self
            .http_health
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("reading response body failed: {e}"))?;
        if status != 200 {
            return Err(format!("HTTP {status} from {url}: {}", clip(&text)));
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
}
