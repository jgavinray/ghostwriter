//! ghostwriter — single-binary MCP server entry point.
//!
//! `ghostwriter [--config <path>] [--self-check]`
//!
//! Without flags the server serves MCP over stdio (rmcp `stdio()`
//! transport). Configuration resolves once at startup: built-in defaults,
//! overlaid by the TOML config file (`--config <path>`, else
//! `~/.config/ghostwriter/config.toml` on macOS and Linux alike), then by
//! `GHOSTWRITER_*` environment variables (see `config::load`). `--self-check`
//! resolves the configuration, prints ONE JSON line
//! `{base_url, model, temperature, timeout_secs}` to stdout, and exits 0 —
//! configuration that cannot resolve is never served with.

use std::path::PathBuf;
use std::process::exit;

use rmcp::ServiceExt;

const USAGE: &str = "usage: ghostwriter [--config <path>] [--self-check]";

#[tokio::main]
async fn main() {
    let mut self_check = false;
    let mut config_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--self-check" => self_check = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                exit(0);
            }
            "--config" => match args.next() {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => {
                    eprintln!("ghostwriter: --config requires a path\n{USAGE}");
                    exit(1);
                }
            },
            _ if arg.starts_with("--config=") => {
                config_path = Some(PathBuf::from(&arg["--config=".len()..]));
            }
            _ => {
                eprintln!("ghostwriter: unknown argument {arg:?}\n{USAGE}");
                exit(1);
            }
        }
    }
    let config = match ghostwriter::config::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(msg) => {
            eprintln!("ghostwriter: {msg}");
            exit(1);
        }
    };
    if self_check {
        println!(
            "{{\"base_url\":{},\"model\":{},\"temperature\":{},\"timeout_secs\":{}}}",
            serde_json::Value::from(config.base_url),
            serde_json::Value::from(config.model),
            config.temperature,
            config.timeout.as_secs(),
        );
        exit(0);
    }
    let server = match ghostwriter::writer::WritingServer::new(config) {
        Ok(server) => server,
        Err(msg) => {
            eprintln!("ghostwriter: {msg}");
            exit(1);
        }
    };
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(err) => {
            eprintln!("ghostwriter: serving failed: {err}");
            exit(1);
        }
    };
    if let Err(err) = running.waiting().await {
        eprintln!("ghostwriter: service stopped: {err}");
        exit(1);
    }
}
