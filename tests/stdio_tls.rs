//! End-to-end stdio pins for the public-distribution contract: the binary
//! a Homebrew install runs must (a) self-check clean on built-in defaults
//! with no internal hostname anywhere, and (b) take an `https://` endpoint
//! all the way to a real TLS connection attempt, reporting the failure as
//! an `isError` tool result — never a JSON-RPC error and never a build
//! without a TLS backend.
//!
//! Both tests spawn the built binary with a cleared environment (no stray
//! `GHOSTWRITER_*`, no `~/.config` interference: `HOME` points at an empty
//! temp dir), exactly the shape of a fresh public install.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ghostwriter"))
}

/// A HOME nothing lives under: config resolution must stand on the
/// built-in defaults alone.
fn empty_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ghostwriter-stdio-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp HOME");
    dir
}

/// `--self-check` on a fresh install: exit 0, one JSON line, the loopback
/// default named verbatim, and no internal hostname anywhere in it. This
/// asserts the shipped binary's behaviour, not a re-typed constant.
#[test]
fn self_check_names_the_loopback_default_and_no_internal_host() {
    let home = empty_home("self-check");
    let out = Command::new(binary())
        .env_clear()
        .env("HOME", &home)
        .arg("--self-check")
        .output()
        .expect("run --self-check");
    assert!(out.status.success(), "--self-check must exit 0");
    let text = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let json: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!("--self-check must print one JSON line, got {text:?}: {e}");
    });
    assert_eq!(json["base_url"], "http://127.0.0.1:8002/v1", "{text}");
    assert_eq!(json["model"], "hemmingway-1", "{text}");
    assert!(!text.contains("hyper03"), "internal host leaked: {text}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The HTTPS pin, with a discriminator that actually discriminates TLS.
/// Two independent guards, both mutation-verified (2026-10-07) against a
/// /tmp copy with `"rustls"` removed from the reqwest feature list in
/// Cargo.toml:
///
/// 1. Compile-time: the builder call below names
///    `ClientBuilder::tls_backend_rustls`, which reqwest 0.13.5 exposes
///    only under its `rustls` feature (`__rustls` cfg). Drop the feature
///    and this test file stops compiling — loudly, not as a passing
///    assertion.
/// 2. Runtime: the model tool is pointed at a plain-TCP endpoint that
///    accepts the connection, lets the client send its ClientHello, then
///    hangs up mid-handshake. Only a build with a real TLS backend ever
///    reaches a handshake: observed here, the rustls build reports the
///    tokio-rustls marker "tls handshake eof", while the no-TLS build
///    never writes to the wire and reports hyper-util's "invalid URL,
///    scheme is not http". The asserted text is therefore the TLS-only
///    marker plus the absence of the no-TLS marker — the old fixed
///    refused-port probe asserted a string both builds produced
///    byte-for-byte and proved nothing.
///
/// The fault must still surface as an `isError` tool result, never a
/// JSON-RPC error. The server either answers or the bounded read fails;
/// a silent server never hangs the suite.
#[test]
fn model_tool_over_https_reports_is_error_connection_fault() {
    // rustls-gated: `tls_backend_rustls` does not exist on the builder
    // without the reqwest `rustls` feature. This call IS the
    // compile-time half of the pin.
    let _ = reqwest::Client::builder().tls_backend_rustls();

    let (port, stop) = spawn_fake_tls_endpoint();
    let home = empty_home("tls");
    let mut child = Command::new(binary())
        .env_clear()
        .env("HOME", &home)
        .env(
            "GHOSTWRITER_BASE_URL",
            format!("https://127.0.0.1:{port}/v1"),
        )
        .env("GHOSTWRITER_MODEL", "tls-probe-model")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ghostwriter");

    let outcome = drive(&mut child);

    // Nothing of ours outlives the test, pass or fail.
    stop.store(true, Ordering::SeqCst);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);
    let line = outcome.expect("model tool answered over stdio");

    assert!(line.contains("\"id\":2"), "wrong response: {line}");
    assert!(
        !line.contains("\"error\""),
        "tool failure leaked into a JSON-RPC error: {line}"
    );
    assert!(
        line.contains("\"isError\":true"),
        "not an isError result: {line}"
    );
    // The connection-fault envelope: the send died before any response
    // byte, so the client's resend ran and the message names the url.
    assert!(
        line.contains("tls-probe-model request failed after 2 attempts")
            && line.contains(&format!(
                "request to https://127.0.0.1:{port}/v1/chat/completions failed"
            )),
        "expected a connection fault against the https url: {line}"
    );
    // The TLS-only half: this text comes from the TLS handshake and
    // cannot appear without a TLS backend.
    assert!(
        line.contains("tls handshake eof"),
        "no TLS handshake was ever attempted: {line}"
    );
    // The no-TLS-build marker, observed byte-for-byte when the feature
    // is removed: its presence means the plain connector refused the
    // https scheme before any wire traffic.
    assert!(
        !line.contains("invalid URL, scheme is not http"),
        "the no-TLS-backend build path is back: {line}"
    );
}

/// A plain-TCP endpoint that is not a TLS server: it accepts, drains
/// the client's ClientHello, then closes without answering the
/// handshake. A build with a TLS backend dies mid-handshake reporting
/// "tls handshake eof"; a build without one refuses the https scheme
/// before writing anything. The drain matters: close with the
/// ClientHello still unread makes the kernel send RST, and the client
/// would report a reset instead of the handshake EOF. Runs until
/// `stop` flips (the client resends once, so two connections arrive).
fn spawn_fake_tls_endpoint() -> (u16, Arc<AtomicBool>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake tls");
    let port = listener.local_addr().expect("fake tls port").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking fake tls");
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    // Accepted sockets inherit the listener's
                    // non-blocking mode; this drain wants real reads.
                    let _ = sock.set_nonblocking(false);
                    let _ = sock.set_read_timeout(Some(Duration::from_millis(150)));
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => continue,
                        }
                    }
                    // FIN, not RST: the reader saw everything.
                    let _ = sock.shutdown(std::net::Shutdown::Write);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });
    (port, stop)
}

fn drive(child: &mut std::process::Child) -> Result<String, String> {
    // A reader thread bounds every wait: stdout lines arrive on a channel
    // and each expectation gets its own deadline.
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
            let owned = std::mem::take(&mut line);
            if tx.send(owned).is_err() {
                break;
            }
        }
    });

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"stdio-tls-pin","version":"0"},"capabilities":{}}}"#,
    );
    let line = recv(&rx, "initialize response")?;
    if !line.contains("\"result\"") {
        return Err(format!("initialize failed: {line}"));
    }

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    // A model tool: rewrite_prose needs no file and reaches the served
    // model immediately — which is the point.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"rewrite_prose","arguments":{"text":"The report covers the release.","max_tokens":16}}}"#,
    );
    drop(stdin);
    recv(&rx, "tools/call response")
}

fn send(stdin: &mut impl Write, msg: &str) {
    writeln!(stdin, "{msg}").expect("write request");
    stdin.flush().expect("flush request");
}

fn recv(rx: &mpsc::Receiver<String>, what: &str) -> Result<String, String> {
    // 90 s: the client resends the failed connection once (500 ms
    // apart), so both attempts plus startup fit easily; a server that
    // never answers is a failure, not a hang.
    rx.recv_timeout(Duration::from_secs(90))
        .map_err(|e| format!("no {what}: {e}"))
}
