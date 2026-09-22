//! OpenAI-compatible chat client for the hemmingway-1 vLLM server, on
//! reqwest. Retry policy: one resend on transport errors, timeouts, HTTP
//! 429, HTTP 5xx, and empty completions; everything else — any other 4xx,
//! an unparseable 2xx body — fails immediately with the server's own
//! explanation (a bad model id will not heal on resend).

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
enum AttemptError {
    /// Transport failure, timeout, HTTP 429, HTTP 5xx — a resend is justified.
    Retryable(String),
    /// Any other 4xx, or an unparseable 2xx body — resending cannot heal it.
    Fatal(String),
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new(cfg: &Config) -> Result<Client, String> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("building HTTP client failed: {e}"))?;
        Ok(Client { http })
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
            "stream": false,
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
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| AttemptError::Retryable(format!("request to {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| AttemptError::Retryable(format!("reading response body failed: {e}")))?;
        if status >= 400 {
            // 429/5xx may heal on resend; any other 4xx will not. Surface
            // the server's own explanation either way.
            let detail = format!("HTTP {status} from {url}: {}", clip(&text));
            return Err(if status == 429 || status >= 500 {
                AttemptError::Retryable(detail)
            } else {
                AttemptError::Fatal(detail)
            });
        }
        // A 2xx body that will not parse is a broken server, not a
        // transient fault.
        parse_completion(&text).map_err(AttemptError::Fatal)
    }

    /// Served-model list for health reporting.
    pub async fn list_models(&self, cfg: &Config) -> Result<Vec<String>, String> {
        let url = format!("{}/models", cfg.base_url);
        let resp = self
            .http
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

fn parse_completion(body: &str) -> Result<Completion, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| format!("unparseable completion body: {}", clip(body)))?;
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| format!("completion body carries no choices: {}", clip(body)))?;
    let text = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let truncated = choice.get("finish_reason").and_then(Value::as_str) == Some("length");
    Ok(Completion { text, truncated })
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

    #[test]
    fn parses_normal_completion() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Done."},"finish_reason":"stop"}]}"#;
        let c = parse_completion(body).unwrap();
        assert_eq!(c.text, "Done.");
        assert!(!c.truncated);
    }

    #[test]
    fn flags_length_truncation() {
        let body =
            r#"{"choices":[{"message":{"content":"cut off here"},"finish_reason":"length"}]}"#;
        let c = parse_completion(body).unwrap();
        assert!(c.truncated);
        assert_eq!(c.text, "cut off here");
    }

    #[test]
    fn rejects_choiceless_body() {
        assert!(parse_completion(r#"{"choices":[]}"#).is_err());
        assert!(parse_completion("not json").is_err());
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

    fn http_config(base_url: String) -> Config {
        Config {
            base_url,
            model: "test-model".into(),
            temperature: 0.3,
            timeout: Duration::from_secs(2),
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
}
