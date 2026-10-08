//! Authenticated loopback JSON-lines control server: one request and reply per line.
//! This is the transport the MCP server (`designcraft-cli mcp --connect`) wraps.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use designcraft_ui_egui::ControlRequest;
use serde_json::{Map, Value, json};

const MAX_CONNECTIONS: usize = 8;
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUESTS_PER_CONNECTION: usize = 4_096;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);
const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 1_024;
const TOKEN_ENV: &str = "DESIGNCRAFT_CONTROL_TOKEN";

/// Start a bounded control server. When no token was configured, a fresh capability token is
/// printed to stderr so a deliberate local client can copy it into `DESIGNCRAFT_CONTROL_TOKEN`.
pub fn start(port: u16, ctx: egui::Context) -> Result<Receiver<ControlRequest>, String> {
    let (token, generated) = control_token()?;
    let (tx, rx) = channel::<ControlRequest>();
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("failed to bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("designcraft: authenticated control server listening on 127.0.0.1:{port}");
    if generated {
        eprintln!("designcraft: control token: {token}");
        eprintln!("designcraft: set {TOKEN_ENV} to this token in clients");
    }
    std::thread::Builder::new()
        .name("designcraft-control-listener".into())
        .spawn(move || accept_connections(listener, tx, ctx, token))
        .map_err(|e| format!("failed to start control listener: {e}"))?;
    Ok(rx)
}

fn control_token() -> Result<(String, bool), String> {
    match std::env::var(TOKEN_ENV) {
        Ok(token) => {
            let len = token.len();
            if !(MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&len) {
                return Err(format!("{TOKEN_ENV} must contain between {MIN_TOKEN_BYTES} and {MAX_TOKEN_BYTES} bytes"));
            }
            Ok((token, false))
        }
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{TOKEN_ENV} must be valid UTF-8")),
        Err(std::env::VarError::NotPresent) => {
            let mut bytes = [0_u8; 32];
            getrandom::fill(&mut bytes).map_err(|e| format!("could not generate a control token: {e}"))?;
            let mut token = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                use std::fmt::Write as _;
                write!(token, "{byte:02x}").map_err(|_| "could not encode the generated control token".to_string())?;
            }
            Ok((token, true))
        }
    }
}

fn accept_connections(listener: TcpListener, tx: Sender<ControlRequest>, ctx: egui::Context, token: String) {
    let active = Arc::new(AtomicUsize::new(0));
    let token: Arc<str> = Arc::from(token);
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(stream) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("designcraft: control listener stopped: {e}");
                break;
            }
        };
        let slot = ConnectionSlot::acquire(Arc::clone(&active));
        let Some(slot) = slot else {
            let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "control server is busy"}));
            continue;
        };
        let tx = tx.clone();
        let ctx = ctx.clone();
        let token = Arc::clone(&token);
        if let Err(e) = std::thread::Builder::new().name("designcraft-control-client".into()).spawn(move || {
            let _slot = slot;
            serve(stream, tx, ctx, &token);
        }) {
            eprintln!("designcraft: could not serve control connection: {e}");
        }
    }
}

struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    fn acquire(active: Arc<AtomicUsize>) -> Option<Self> {
        active.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_CONNECTIONS).then_some(n + 1)).ok().map(|_| Self(active))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve(stream: TcpStream, tx: Sender<ControlRequest>, ctx: egui::Context, token: &str) {
    if stream.set_read_timeout(Some(SOCKET_TIMEOUT)).is_err() || stream.set_write_timeout(Some(SOCKET_TIMEOUT)).is_err() {
        return;
    }
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read);
    let mut out = stream;
    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        let mut line = Vec::new();
        let read = reader.by_ref().take((MAX_REQUEST_BYTES + 1) as u64).read_until(b'\n', &mut line);
        let Ok(bytes_read) = read else { break };
        if bytes_read == 0 {
            break;
        }
        let line = match std::str::from_utf8(&line) {
            Ok(line) => line,
            Err(_) => {
                let _ = write_reply(&mut out, json!({"ok": false, "error": "control request must be valid UTF-8"}));
                break;
            }
        };
        if looks_like_http(line) {
            let _ = out.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            break;
        }
        if bytes_read > MAX_REQUEST_BYTES || !line.ends_with('\n') {
            let _ = write_reply(&mut out, json!({"ok": false, "error": "invalid control request framing"}));
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut parsed = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(msg)) => msg,
            Ok(_) => {
                let _ = write_reply(&mut out, json!({"ok": false, "error": "control request must be a JSON object"}));
                break;
            }
            Err(e) => {
                let _ = write_reply(&mut out, json!({"ok": false, "error": format!("bad JSON: {e}")}));
                break;
            }
        };
        let id = match parsed.remove("id") {
            Some(Value::Number(id)) => Value::Number(id),
            Some(Value::String(id)) if id.len() <= 128 => Value::String(id),
            _ => Value::Null,
        };
        if !request_is_authenticated(&parsed, token) {
            let _ = write_reply(&mut out, json!({"id": id, "ok": false, "error": "unauthorized"}));
            break;
        }
        let method = match parsed.remove("method") {
            Some(Value::String(method)) if !method.is_empty() && method.len() <= 256 => method,
            _ => {
                let _ = write_reply(&mut out, json!({"id": id, "ok": false, "error": "invalid control method"}));
                break;
            }
        };
        let params = parsed.remove("params").unwrap_or_else(|| json!({}));
        let (req, rrx) = ControlRequest::new(method, params);
        if tx.send(req).is_err() {
            break;
        }
        ctx.request_repaint();
        let mut reply = rrx.recv_timeout(APP_REPLY_TIMEOUT).unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}));
        if let Some(object) = reply.as_object_mut() {
            object.insert("id".into(), id);
        }
        if write_reply(&mut out, reply).is_err() {
            break;
        }
    }
}

fn request_is_authenticated(msg: &Map<String, Value>, expected: &str) -> bool {
    msg.get("token").and_then(Value::as_str).is_some_and(|candidate| constant_time_eq(candidate.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut different = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        different |= usize::from(candidate.get(index).copied().unwrap_or(0) ^ expected_byte);
    }
    different == 0
}

fn looks_like_http(line: &str) -> bool {
    let line = line.trim_start();
    ["GET ", "POST ", "PUT ", "PATCH ", "DELETE ", "OPTIONS ", "HEAD ", "CONNECT ", "TRACE ", "PRI "].iter().any(|method| line.starts_with(method))
        || line.starts_with("HTTP/")
}

fn write_reply(out: &mut TcpStream, reply: Value) -> std::io::Result<()> {
    writeln!(out, "{reply}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn spawn_server(token: &'static str) -> (TcpStream, Receiver<ControlRequest>) {
        let (client, server) = connected_pair();
        let (tx, rx) = channel();
        std::thread::spawn(move || serve(server, tx, egui::Context::default(), token));
        (client, rx)
    }

    #[test]
    fn authenticated_request_is_forwarded() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        writeln!(client, "{}", json!({"id": 7, "token": "this-test-token-is-at-least-32-bytes", "method": "ui.inspect"})).unwrap();
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(req.method, "ui.inspect");
        req.reply.send(json!({"ok": true, "result": {"ready": true}})).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!((reply["id"].as_u64(), reply["ok"].as_bool()), (Some(7), Some(true)));
    }

    #[test]
    fn missing_token_is_rejected_before_dispatch() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        writeln!(client, "{}", json!({"id": 8, "method": "ui.inspect"})).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"], "unauthorized");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn http_framing_gets_an_http_error_and_no_dispatch() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn request_must_use_object_framing() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        writeln!(client, "[]").unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"], "control request must be a JSON object");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn request_must_be_utf8() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        client.write_all(&[0xff, b'\n']).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"], "control request must be valid UTF-8");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn oversized_line_is_rejected_before_dispatch() {
        let (mut client, rx) = spawn_server("this-test-token-is-at-least-32-bytes");
        let mut line = vec![b'x'; MAX_REQUEST_BYTES];
        line.push(b'\n');
        client.write_all(&line).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"], "invalid control request framing");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn token_comparison_checks_content_and_length() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"different"));
        assert!(!constant_time_eq(b"same-extra", b"same"));
    }

    #[test]
    fn connection_slots_are_bounded_and_reusable() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut slots = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            slots.push(ConnectionSlot::acquire(Arc::clone(&active)).unwrap());
        }
        assert!(ConnectionSlot::acquire(Arc::clone(&active)).is_none());
        drop(slots.pop());
        assert!(ConnectionSlot::acquire(active).is_some());
    }
}
