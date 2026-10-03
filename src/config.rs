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
//!    `GHOSTWRITER_IDLE_TIMEOUT_SECS`, `GHOSTWRITER_STYLE`, so the mcp.json
//!    `env` block or a systemd `Environment=` can override the file.
//!    `timeout_secs` bounds one whole generation, queueing included (a busy
//!    engine holds a queued request in total silence until the first token);
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

/// Largest source payload read from disk, in bytes: 400,000 is roughly
/// 100k tokens at a four-bytes-per-token heuristic (code ASCII skews
/// denser, prose lighter). Together with `max_tokens` up to
/// `MAX_TOKENS_CAP`, a maximal payload CAN exceed the 131072-token
/// served context; the served engine then rejects the request with a 4xx
/// that surfaces verbatim as an `isError` tool result — never silent.
/// Larger files are truncated WITH a visible marker — silent truncation
/// would make the model document half a file and call it complete.
pub const MAX_SOURCE_BYTES: usize = 400_000;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
    pub timeout: Duration,
    pub idle_timeout: Duration,
    /// Server-side default for the per-call `style` argument: the id of a
    /// registered standard (`crate::styles`), validated at startup, or
    /// `none` for the house style alone. A call that omits `style` gets
    /// this; an explicit `style` from the caller always wins.
    pub style: String,
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
    style: Option<String>,
    /// Retired: replaced by `style`. Declared so the key reaches a
    /// helpful startup message instead of the unknown-key refusal.
    ste: Option<bool>,
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
    if env_present("GHOSTWRITER_STE") {
        return Err(format!(
            "GHOSTWRITER_STE is retired: `style` replaced `ste` — set \
             GHOSTWRITER_STYLE to one of: {} (the old true maps to \"ste\")",
            crate::styles::usage()
        ));
    }
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
    if file.ste.is_some() {
        return Err(format!(
            "the config key `ste` was replaced by `style`: set style = \\\"ste\\\" \
             to keep ASD-STE100 as the default (valid: {})",
            crate::styles::usage()
        ));
    }
    build(
        env_str("GHOSTWRITER_BASE_URL").or(file.base_url),
        env_str("GHOSTWRITER_MODEL").or(file.model),
        env_f32("GHOSTWRITER_TEMPERATURE")?.or(file.temperature),
        env_u64("GHOSTWRITER_TIMEOUT_SECS")?.or(file.timeout_secs),
        env_u64("GHOSTWRITER_IDLE_TIMEOUT_SECS")?.or(file.idle_timeout_secs),
        env_str("GHOSTWRITER_STYLE").or(file.style),
    )
}

/// Merge one optional value per field over the defaults and validate.
fn build(
    base_url: Option<String>,
    model: Option<String>,
    temperature: Option<f32>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
    style: Option<String>,
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
    // A timeout past one day is a typo, not a knob; refuse it at startup
    // naming the cap — silently clamping config is this repo's enemy.
    const MAX_TIMEOUT_SECS: u64 = 86_400;
    let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout_secs > MAX_TIMEOUT_SECS {
        return Err(format!(
            "timeout_secs must not exceed {MAX_TIMEOUT_SECS} (one day): {timeout_secs}"
        ));
    }
    let idle_timeout_secs = idle_timeout_secs.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    if idle_timeout_secs > MAX_TIMEOUT_SECS {
        return Err(format!(
            "idle_timeout_secs must not exceed {MAX_TIMEOUT_SECS} (one day): {idle_timeout_secs}"
        ));
    }
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let idle_timeout = Duration::from_secs(idle_timeout_secs.max(1));
    if idle_timeout >= timeout {
        return Err(format!(
            "idle_timeout ({idle_timeout:?}) must be shorter than timeout ({timeout:?}): \
             the idle kill would never fire before the overall deadline"
        ));
    }
    // The default standard must name a registered style: a typo dies at
    // startup, not silently in every prompt at serve time.
    let style = style
        .unwrap_or_else(|| crate::styles::NONE_ID.to_string())
        .trim()
        .to_ascii_lowercase();
    if style != crate::styles::NONE_ID && crate::styles::lookup(&style).is_none() {
        return Err(format!(
            "unknown style {style:?}; expected one of: {}",
            crate::styles::usage()
        ));
    }
    Ok(Config {
        base_url,
        model,
        temperature: temperature.clamp(0.0, 2.0),
        timeout,
        idle_timeout,
        style,
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
        // An exported-but-empty value means "unset", as in `env_str` and
        // `env_present` — an empty GHOSTWRITER_TEMPERATURE falls back to
        // the default instead of dying at startup.
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => value
            .parse::<f32>()
            .map(Some)
            .map_err(|_| format!("{key} is not a number: {value}")),
        Err(_) => Ok(None),
    }
}

fn env_u64(key: &str) -> Result<Option<u64>, String> {
    match std::env::var(key) {
        // Empty means "unset" — see `env_f32`.
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("{key} is not a number: {value}")),
        Err(_) => Ok(None),
    }
}

fn env_present(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve() {
        let c = build(None, None, None, None, None, None).unwrap();
        assert_eq!(c.base_url, "http://hyper03:8002/v1");
        assert_eq!(c.model, "hemmingway-1");
        assert_eq!(c.temperature, 0.3);
        assert_eq!(c.timeout, Duration::from_secs(900));
        assert_eq!(c.idle_timeout, Duration::from_secs(60));
        assert_eq!(c.style, "none");
    }

    #[test]
    fn build_applies_and_validates_overrides() {
        let c = build(
            Some("http://box:9/v1/".into()),
            Some("m".into()),
            Some(9.0),
            Some(2),
            Some(0),
            Some("STE".into()),
        )
        .unwrap();
        assert_eq!(c.base_url, "http://box:9/v1");
        assert_eq!(c.temperature, 2.0); // clamped into 0.0..2.0
        assert_eq!(c.timeout, Duration::from_secs(2));
        assert_eq!(c.idle_timeout, Duration::from_secs(1)); // floored at 1
        assert_eq!(c.style, "ste"); // normalized to the registered id
        assert!(build(Some("ftp://box".into()), None, None, None, None, None).is_err());
        assert!(build(None, Some(String::new()), None, None, None, None).is_err());
        assert!(build(None, None, Some(f32::NAN), None, None, None).is_err());
        assert!(build(None, None, Some(f32::INFINITY), None, None, None).is_err());
    }

    /// An idle kill that can never fire before the overall deadline is a
    /// dead knob; refuse it at startup, where the typo lives.
    #[test]
    fn idle_must_lose_the_race_to_timeout() {
        assert!(build(None, None, None, Some(30), Some(60), None).is_err());
        assert!(build(None, None, None, Some(60), Some(60), None).is_err());
        assert!(build(None, None, None, Some(61), Some(60), None).is_ok());
    }

    #[test]
    fn config_file_parses_partial_and_refuses_unknown_keys() {
        let partial: FileConfig = toml::from_str("model = \"other-model\"\n").unwrap();
        assert_eq!(partial.model.as_deref(), Some("other-model"));
        assert!(partial.base_url.is_none());

        let full: FileConfig = toml::from_str(
            "base_url = \"http://box:9/v1\"\nmodel = \"m\"\ntemperature = 0.1\ntimeout_secs = 60\nidle_timeout_secs = 5\nstyle = \"google\"\n",
        )
        .unwrap();
        assert_eq!(full.base_url.as_deref(), Some("http://box:9/v1"));
        assert_eq!(full.temperature, Some(0.1));
        assert_eq!(full.timeout_secs, Some(60));
        assert_eq!(full.idle_timeout_secs, Some(5));
        assert_eq!(full.style.as_deref(), Some("google"));

        assert!(toml::from_str::<FileConfig>("mode1 = \"typo\"").is_err());
    }

    #[test]
    fn explicit_config_must_exist() {
        let _g = load_lock();
        let missing = std::env::temp_dir().join("ghostwriter-no-such-config.toml");
        let err = load(Some(&missing)).unwrap_err();
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    #[test]
    fn style_defaults_to_none_and_validates_registered_ids() {
        assert_eq!(
            build(None, None, None, None, None, None).unwrap().style,
            "none"
        );
        assert_eq!(
            build(None, None, None, None, None, Some(" Microsoft ".into()))
                .unwrap()
                .style,
            "microsoft"
        );
        let err = build(None, None, None, None, None, Some("chicago".into())).unwrap_err();
        assert!(err.contains("diataxis"), "{err}");
        let f: FileConfig = toml::from_str("style = \"ste\"\n").unwrap();
        assert_eq!(f.style.as_deref(), Some("ste"));
    }

    /// The retired `ste` key must reach a message that names the repair,
    /// not the cold `deny_unknown_fields` refusal.
    #[test]
    fn retired_ste_config_key_names_the_replacement() {
        let _g = load_lock();
        let dir = std::env::temp_dir().join("ghostwriter-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("legacy.toml");
        std::fs::write(&file, "ste = true\n").unwrap();
        let err = load(Some(&file)).unwrap_err();
        assert!(err.contains("`style`"), "{err}");
        assert!(err.contains("ste"), "{err}");
    }

    #[test]
    fn env_present_detects_nonempty_values() {
        assert!(!env_present("GHOSTWRITER_TEST_PRESENT"));
        std::env::set_var("GHOSTWRITER_TEST_PRESENT", "");
        assert!(!env_present("GHOSTWRITER_TEST_PRESENT"));
        std::env::set_var("GHOSTWRITER_TEST_PRESENT", "ste");
        assert!(env_present("GHOSTWRITER_TEST_PRESENT"));
        std::env::remove_var("GHOSTWRITER_TEST_PRESENT");
    }
    /// load() reads process env; every test that sets a real GHOSTWRITER_*
    /// variable and calls load must take turns, or a sibling sees the
    /// variable leak in.
    fn load_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The env half of the retirement: a stale deployment exporting
    /// GHOSTWRITER_STE must be refused with the repair named, never
    /// silently ignored.
    #[test]
    fn retired_ghostwriter_ste_env_names_the_replacement() {
        let _g = load_lock();
        std::env::set_var("GHOSTWRITER_STE", "true");
        let missing = std::env::temp_dir().join("ghostwriter-no-such-config-ste.toml");
        let err = load(Some(&missing)).unwrap_err();
        std::env::remove_var("GHOSTWRITER_STE");
        assert!(err.contains("GHOSTWRITER_STE is retired"), "{err}");
        assert!(err.contains("GHOSTWRITER_STYLE"), "{err}");
    }

    /// GHOSTWRITER_STYLE must win over the config file's style key, and
    /// the file value must stand when the environment is quiet.
    #[test]
    fn ghostwriter_style_env_overrides_the_file() {
        let _g = load_lock();
        let dir = std::env::temp_dir().join("ghostwriter-style-env-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "style = \"none\"\n").unwrap();
        std::env::set_var("GHOSTWRITER_STYLE", "google");
        let c = load(Some(&file)).unwrap();
        std::env::remove_var("GHOSTWRITER_STYLE");
        assert_eq!(c.style, "google");
        let c = load(Some(&file)).unwrap();
        assert_eq!(c.style, "none");
    }

    /// N1: an exported-but-empty `GHOSTWRITER_TEMPERATURE` /
    /// `GHOSTWRITER_TIMEOUT_SECS` / `GHOSTWRITER_IDLE_TIMEOUT_SECS` means
    /// "unset" — the same rule `env_str` and `env_present` apply — so the
    /// default stands instead of dying at startup. A non-empty garbage
    /// value still dies, naming the key.
    #[test]
    fn empty_numeric_env_values_fall_back_to_defaults() {
        let _g = load_lock();
        struct ClearEnv(String);
        impl Drop for ClearEnv {
            fn drop(&mut self) {
                std::env::remove_var("GHOSTWRITER_TEMPERATURE");
                std::env::remove_var("GHOSTWRITER_TIMEOUT_SECS");
                std::env::remove_var("GHOSTWRITER_IDLE_TIMEOUT_SECS");
                std::env::set_var("HOME", &self.0);
            }
        }
        // Point HOME at an empty directory so load(None) resolves the
        // defaults with no config file in the way.
        let home = std::env::var("HOME").unwrap_or_default();
        let isolation = std::env::temp_dir().join("ghostwriter-empty-env-test");
        std::fs::create_dir_all(&isolation).unwrap();
        std::env::set_var("HOME", &isolation);
        let _clear = ClearEnv(home);
        std::env::set_var("GHOSTWRITER_TEMPERATURE", "");
        std::env::set_var("GHOSTWRITER_TIMEOUT_SECS", "");
        std::env::set_var("GHOSTWRITER_IDLE_TIMEOUT_SECS", "");
        let c = load(None).expect("empty numeric env values must fall back to defaults");
        assert_eq!(c.temperature, DEFAULT_TEMPERATURE);
        assert_eq!(c.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(
            c.idle_timeout,
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );

        // The contrast: a non-empty garbage value is still a loud startup
        // error naming the key and the offending text.
        std::env::set_var("GHOSTWRITER_TEMPERATURE", "scalding");
        let err = load(None).unwrap_err();
        std::env::remove_var("GHOSTWRITER_TEMPERATURE");

        assert!(
            err.contains("GHOSTWRITER_TEMPERATURE is not a number: scalding"),
            "{err}"
        );
        std::env::set_var("GHOSTWRITER_TIMEOUT_SECS", "soon");
        let err = load(None).unwrap_err();

        assert!(
            err.contains("GHOSTWRITER_TIMEOUT_SECS is not a number: soon"),
            "{err}"
        );
    }

    /// N2: an absurd timeout is a startup error that names the cap —
    /// never a silent clamp — while one day exactly still resolves.
    #[test]
    fn timeouts_are_capped_at_one_day() {
        let cap_ok = build(None, None, None, Some(86_400), Some(60), None)
            .expect("timeout_secs exactly at the cap must resolve");
        assert_eq!(cap_ok.timeout, Duration::from_secs(86_400));

        let err = build(None, None, None, Some(86_401), None, None).unwrap_err();
        assert!(err.contains("timeout_secs must not exceed"), "{err}");
        assert!(err.contains("86_400") || err.contains("86400"), "{err}");
        let err = build(None, None, None, None, Some(86_401), None).unwrap_err();
        assert!(err.contains("idle_timeout_secs must not exceed"), "{err}");
        assert!(err.contains("86_400") || err.contains("86400"), "{err}");

        // Both at the cap are individually allowed, so an over-long idle
        // is refused by the idle<timeout rule, not the cap — the two
        // messages stay distinct.
        let err = build(None, None, None, Some(86_400), Some(86_400), None).unwrap_err();
        assert!(err.contains("must be shorter than timeout"), "{err}");
        assert!(!err.contains("must not exceed"), "{err}");
    }
}
