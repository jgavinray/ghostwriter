//! Runtime configuration, resolved once at startup — never re-read while
//! serving.
//!
//! Layering, lowest to highest:
//!
//! 1. Built-in defaults (the fleet hemmingway-1 box).
//! 2. Config file, TOML: `~/.config/ghostwriter/config.toml` — the same
//!    path on macOS and Linux — or the path given with `--config`. A
//!    missing default file is fine and the defaults stand; an explicit
//!    `--config` that does not exist is a startup error.
//! 3. Environment: `GHOSTWRITER_BASE_URL`, `GHOSTWRITER_MODEL`,
//!    `GHOSTWRITER_TEMPERATURE`, `GHOSTWRITER_TIMEOUT_SECS`,
//!    `GHOSTWRITER_IDLE_TIMEOUT_SECS`, so the mcp.json `env` block or a
//!    systemd `Environment=` can override the file. `timeout_secs` bounds
//!    one whole generation, queueing included (a busy engine holds a
//!    queued request in total silence until the first token);
//!    `idle_timeout_secs` kills a stream only after it began emitting and
//!    then went silent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_BASE_URL: &str = "http://hyper03:8002/v1";
pub const DEFAULT_MODEL: &str = "hemmingway-1";
pub const DEFAULT_TEMPERATURE: f32 = 0.3;
/// Overall budget for one generation attempt (request sent to last
/// token). Long enough for a full 4096-token rewrite on a contended
/// engine; the caller's MCP timeout must exceed this.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
/// Silence that declares an already-streaming response dead. Queueing
/// before the first token is exempt: it waits only on `timeout`. A
/// healthy decode at even 1 tok/s beats this; a wedged engine does not.
/// Also budgets health probes.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 60;

/// Hard ceiling on `max_tokens` any single call may request. The served
/// model's context is 131072 tokens; 32768 leaves room for source payloads.
pub const MAX_TOKENS_CAP: u32 = 32768;

/// Largest source payload read from disk, in bytes (~30k tokens). Larger
/// files are truncated WITH a visible marker — silent truncation would
/// make the model document half a file and call it complete.
pub const MAX_SOURCE_BYTES: usize = 400_000;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
    pub timeout: Duration,
    pub idle_timeout: Duration,
}

/// The config-file schema. Every field is optional — a file may set only
/// what it wants to move off the defaults — and unknown keys are refused,
/// so a typo (`mode1`) dies at startup instead of silently using defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    base_url: Option<String>,
    model: Option<String>,
    temperature: Option<f32>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
}

/// The single config-file location, identical on macOS and Linux:
/// `~/.config/ghostwriter/config.toml`.
fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/ghostwriter/config.toml"))
}

/// Resolve the configuration: defaults, overlaid by the config file, then
/// by the environment. `explicit` is the `--config` path when given.
pub fn load(explicit: Option<&Path>) -> Result<Config, String> {
    let path = explicit.map(Path::to_path_buf).or_else(default_config_path);
    let file = match &path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Some(
                toml::from_str::<FileConfig>(&text)
                    .map_err(|e| format!("parsing config file {} failed: {e}", path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if explicit.is_some() {
                    return Err(format!("config file {} does not exist", path.display()));
                }
                None
            }
            Err(e) => {
                return Err(format!(
                    "reading config file {} failed: {e}",
                    path.display()
                ))
            }
        },
        None => None,
    };
    let file = file.unwrap_or_default();
    build(
        env_str("GHOSTWRITER_BASE_URL").or(file.base_url),
        env_str("GHOSTWRITER_MODEL").or(file.model),
        env_f32("GHOSTWRITER_TEMPERATURE")?.or(file.temperature),
        env_u64("GHOSTWRITER_TIMEOUT_SECS")?.or(file.timeout_secs),
        env_u64("GHOSTWRITER_IDLE_TIMEOUT_SECS")?.or(file.idle_timeout_secs),
    )
}

/// Merge one optional value per field over the defaults and validate.
fn build(
    base_url: Option<String>,
    model: Option<String>,
    temperature: Option<f32>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
) -> Result<Config, String> {
    let base_url = base_url
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(format!(
            "base_url must start with http:// or https://: {base_url}"
        ));
    }
    let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    if model.is_empty() {
        return Err("model must not be empty".to_string());
    }
    let temperature = temperature.unwrap_or(DEFAULT_TEMPERATURE);
    if !temperature.is_finite() {
        return Err(format!(
            "temperature must be a finite number: {temperature}"
        ));
    }
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
    let idle_timeout = Duration::from_secs(
        idle_timeout_secs
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS)
            .max(1),
    );
    if idle_timeout >= timeout {
        return Err(format!(
            "idle_timeout ({idle_timeout:?}) must be shorter than timeout ({timeout:?}): \
             the idle kill would never fire before the overall deadline"
        ));
    }
    Ok(Config {
        base_url,
        model,
        temperature: temperature.clamp(0.0, 2.0),
        timeout,
        idle_timeout,
    })
}

fn env_str(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn env_f32(key: &str) -> Result<Option<f32>, String> {
    match std::env::var(key) {
        Ok(value) => value
            .parse::<f32>()
            .map(Some)
            .map_err(|_| format!("{key} is not a number: {value}")),
        Err(_) => Ok(None),
    }
}

fn env_u64(key: &str) -> Result<Option<u64>, String> {
    match std::env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("{key} is not a number: {value}")),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve() {
        let c = build(None, None, None, None, None).unwrap();
        assert_eq!(c.base_url, "http://hyper03:8002/v1");
        assert_eq!(c.model, "hemmingway-1");
        assert_eq!(c.temperature, 0.3);
        assert_eq!(c.timeout, Duration::from_secs(900));
        assert_eq!(c.idle_timeout, Duration::from_secs(60));
    }

    #[test]
    fn build_applies_and_validates_overrides() {
        let c = build(
            Some("http://box:9/v1/".into()),
            Some("m".into()),
            Some(9.0),
            Some(2),
            Some(0),
        )
        .unwrap();
        assert_eq!(c.base_url, "http://box:9/v1");
        assert_eq!(c.temperature, 2.0); // clamped into 0.0..2.0
        assert_eq!(c.timeout, Duration::from_secs(2));
        assert_eq!(c.idle_timeout, Duration::from_secs(1)); // floored at 1
        assert!(build(Some("ftp://box".into()), None, None, None, None).is_err());
        assert!(build(None, Some(String::new()), None, None, None).is_err());
        assert!(build(None, None, Some(f32::NAN), None, None).is_err());
        assert!(build(None, None, Some(f32::INFINITY), None, None).is_err());
    }

    /// An idle kill that can never fire before the overall deadline is a
    /// dead knob; refuse it at startup, where the typo lives.
    #[test]
    fn idle_must_lose_the_race_to_timeout() {
        assert!(build(None, None, None, Some(30), Some(60)).is_err());
        assert!(build(None, None, None, Some(60), Some(60)).is_err());
        assert!(build(None, None, None, Some(61), Some(60)).is_ok());
    }

    #[test]
    fn config_file_parses_partial_and_refuses_unknown_keys() {
        let partial: FileConfig = toml::from_str("model = \"other-model\"\n").unwrap();
        assert_eq!(partial.model.as_deref(), Some("other-model"));
        assert!(partial.base_url.is_none());

        let full: FileConfig = toml::from_str(
            "base_url = \"http://box:9/v1\"\nmodel = \"m\"\ntemperature = 0.1\ntimeout_secs = 60\nidle_timeout_secs = 5\n",
        )
        .unwrap();
        assert_eq!(full.base_url.as_deref(), Some("http://box:9/v1"));
        assert_eq!(full.temperature, Some(0.1));
        assert_eq!(full.timeout_secs, Some(60));
        assert_eq!(full.idle_timeout_secs, Some(5));

        assert!(toml::from_str::<FileConfig>("mode1 = \"typo\"").is_err());
    }

    #[test]
    fn explicit_config_must_exist() {
        let missing = std::env::temp_dir().join("ghostwriter-no-such-config.toml");
        let err = load(Some(&missing)).unwrap_err();
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }
}
