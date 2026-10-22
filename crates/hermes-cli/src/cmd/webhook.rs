//! `hermes webhook` — standalone incoming webhook server.
//!
//! Minimal HTTP/1.1 server over raw TCP (`tokio::net::TcpListener`); the fork
//! has no axum/warp/hyper dependency, so request parsing is done manually the
//! same way `hermes-gateway`'s webhook adapter does it.
//!
//! `POST /hook` with `{"text": "...", "session": "optional"}` runs one agent
//! turn and replies `{"response": "..."}`. Per-`session` conversation history
//! is kept in memory so repeated webhooks continue the same thread.

use std::collections::HashMap;
use std::sync::Arc;

use hermes_cli::cli::Cli;
use hermes_cli::App;
use hermes_core::{AgentError, Message, MessageRole};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Max bytes buffered while waiting for the HTTP header block.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Max request body we will buffer.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Session key used when the payload omits `session`.
const DEFAULT_SESSION: &str = "webhook";

/// Incoming `POST /hook` payload.
#[derive(Debug, Deserialize)]
struct HookRequest {
    text: String,
    #[serde(default)]
    session: Option<String>,
}

/// Outgoing JSON body for success / error replies.
#[derive(Debug, Serialize)]
struct HookReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A fully-read HTTP request: header block plus exactly `Content-Length` body bytes.
struct RawRequest {
    head: String,
    body: Vec<u8>,
}

/// Failure while serving one HTTP request; maps directly to a JSON error response.
#[derive(Debug)]
struct RequestError {
    status: u16,
    reason: &'static str,
    message: String,
}

impl RequestError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            reason: "Bad Request",
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: 404,
            reason: "Not Found",
            message: "not found".to_string(),
        }
    }

    fn method_not_allowed() -> Self {
        Self {
            status: 405,
            reason: "Method Not Allowed",
            message: "method not allowed, use POST".to_string(),
        }
    }

    fn entity_too_large(message: impl Into<String>) -> Self {
        Self {
            status: 413,
            reason: "Payload Too Large",
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            reason: "Internal Server Error",
            message: message.into(),
        }
    }
}

/// Shared server state: the CLI app (holds the agent loop) plus a per-session
/// conversation transcript so repeated webhooks keep context.
struct WebhookState {
    app: App,
    sessions: Mutex<HashMap<String, Vec<Message>>>,
}

/// Run the standalone webhook server until Ctrl-C.
pub(crate) async fn cmd_webhook(cli: &Cli, port: u16) -> Result<(), AgentError> {
    let app = App::new(cli.clone()).await?;
    let state = Arc::new(WebhookState {
        app,
        sessions: Mutex::new(HashMap::new()),
    });

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| AgentError::Io(format!("failed to bind {addr}: {e}")))?;

    println!("Webhook listening on 0.0.0.0:{port} (POST /hook). Press Ctrl-C to stop.");
    info!("webhook server listening on {addr}");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            handle_connection(stream, state).await;
                        });
                    }
                    Err(e) => warn!("webhook accept error: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down webhook server...");
                break;
            }
        }
    }
    Ok(())
}

/// Serve a single TCP connection: read one HTTP request, route it, reply, close.
async fn handle_connection(stream: TcpStream, state: Arc<WebhookState>) {
    let (mut reader, mut writer) = stream.into_split();
    match serve_request(&mut reader, &mut writer, &state).await {
        Ok(()) => {}
        Err(e) => {
            debug!("webhook request failed: {}", e.message);
            let response = json_error_response(e.status, e.reason, &e.message);
            let _ = writer.write_all(response.as_bytes()).await;
        }
    }
}

async fn serve_request(
    reader: &mut (impl AsyncReadExt + Unpin),
    writer: &mut (impl AsyncWriteExt + Unpin),
    state: &WebhookState,
) -> Result<(), RequestError> {
    let Some(raw) = read_full_request(reader).await? else {
        return Ok(()); // client closed the connection before sending anything
    };
    let (method, path) = parse_request_line(&raw.head)
        .ok_or_else(|| RequestError::bad_request("malformed request line"))?;

    let response = match route_request(&method, &path) {
        Route::Hook => handle_hook(&raw.body, state).await?,
        Route::MethodNotAllowed => return Err(RequestError::method_not_allowed()),
        Route::NotFound => return Err(RequestError::not_found()),
    };

    writer
        .write_all(response.as_bytes())
        .await
        .map_err(|e| RequestError::internal(format!("write error: {e}")))?;
    let _ = writer.shutdown().await;
    Ok(())
}

/// Pure routing decision, kept separate so it is unit-testable without an `App`.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Hook,
    MethodNotAllowed,
    NotFound,
}

fn route_request(method: &str, path: &str) -> Route {
    if path == "/hook" {
        if method == "POST" {
            Route::Hook
        } else {
            Route::MethodNotAllowed
        }
    } else {
        Route::NotFound
    }
}

/// Run one `POST /hook` payload through the agent and build the HTTP response.
async fn handle_hook(body: &[u8], state: &WebhookState) -> Result<String, RequestError> {
    let req: HookRequest = serde_json::from_slice(body)
        .map_err(|e| RequestError::bad_request(format!("invalid JSON: {e}")))?;
    let text = req.text.trim().to_string();
    if text.is_empty() {
        return Err(RequestError::bad_request("missing \"text\" field"));
    }
    let session_key = req
        .session
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SESSION.to_string());

    // Clone the transcript, run the agent without holding the lock, then merge back.
    let history = state
        .sessions
        .lock()
        .await
        .get(&session_key)
        .cloned()
        .unwrap_or_default();
    let mut messages = history;
    messages.push(Message::user(&text));

    let result = state.app.agent.run(messages, None).await.map_err(|e| {
        warn!("webhook agent run failed: {e}");
        RequestError::internal(format!("agent error: {e}"))
    })?;
    let reply = last_assistant_text(&result.messages);

    state
        .sessions
        .lock()
        .await
        .insert(session_key, strip_leading_system_messages(result.messages));

    Ok(json_ok_response(&reply))
}

/// Read one HTTP/1.1 request: headers until the blank line, then exactly
/// `Content-Length` body bytes (looping so split TCP segments are reassembled).
async fn read_full_request(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<Option<RawRequest>, RequestError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];

    let header_end = loop {
        let n = reader
            .read(&mut tmp)
            .await
            .map_err(|e| RequestError::internal(format!("read error: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(RequestError::entity_too_large("headers too large"));
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = parse_content_length(&head);
    if content_length > MAX_BODY_BYTES {
        return Err(RequestError::entity_too_large("body too large"));
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = reader
            .read(&mut tmp)
            .await
            .map_err(|e| RequestError::internal(format!("read error: {e}")))?;
        if n == 0 {
            break; // client closed early; answer with what we have
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > MAX_BODY_BYTES {
            return Err(RequestError::entity_too_large("body too large"));
        }
    }
    body.truncate(content_length);
    Ok(Some(RawRequest { head, body }))
}

/// Parse the request line into `(METHOD, PATH)`; the query string is stripped.
fn parse_request_line(head: &str) -> Option<(String, String)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    Some((method, path))
}

/// Index just past the first blank line (`\r\n\r\n`, tolerating bare `\n\n`).
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

/// Case-insensitive `Content-Length` lookup over the header block; 0 when absent/invalid.
fn parse_content_length(head: &str) -> usize {
    head.lines()
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// Build a complete HTTP/1.1 response with a JSON body (`Connection: close`).
fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

fn json_ok_response(response: &str) -> String {
    let body = serde_json::to_string(&HookReply {
        response: Some(response.to_string()),
        error: None,
    })
    .unwrap_or_else(|_| "{\"response\":\"\"}".to_string());
    http_response(200, "OK", &body)
}

fn json_error_response(status: u16, reason: &str, message: &str) -> String {
    let body = serde_json::to_string(&HookReply {
        response: None,
        error: Some(message.to_string()),
    })
    .unwrap_or_else(|_| "{\"error\":\"internal error\"}".to_string());
    http_response(status, reason, &body)
}

/// Last assistant message text, mirroring the `hermes chat` one-shot extraction.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| {
            if m.role == MessageRole::Assistant {
                m.content.clone()
            } else {
                None
            }
        })
        .unwrap_or_else(|| "(no assistant reply)".to_string())
}

/// `AgentLoop` returns full context including an injected leading system prompt;
/// drop the leading `System` rows so the stored transcript stays clean (same as `App`).
fn strip_leading_system_messages(mut msgs: Vec<Message>) -> Vec<Message> {
    let first_non_system = msgs
        .iter()
        .position(|m| m.role != MessageRole::System)
        .unwrap_or(msgs.len());
    if first_non_system > 0 {
        msgs.drain(0..first_non_system);
    }
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn parse_request_line_basic() {
        let (method, path) = parse_request_line("POST /hook HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/hook");
    }

    #[test]
    fn parse_request_line_strips_query() {
        let (method, path) = parse_request_line("GET /hook?token=abc HTTP/1.0\n").unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/hook");
    }

    #[test]
    fn parse_request_line_malformed() {
        assert!(parse_request_line("").is_none());
        assert!(parse_request_line("GARBAGE").is_none());
    }

    #[test]
    fn route_request_paths() {
        assert_eq!(route_request("POST", "/hook"), Route::Hook);
        assert_eq!(route_request("GET", "/hook"), Route::MethodNotAllowed);
        assert_eq!(route_request("POST", "/other"), Route::NotFound);
        assert_eq!(route_request("GET", "/"), Route::NotFound);
    }

    #[test]
    fn find_header_end_detects_crlf() {
        let buf = b"POST /hook HTTP/1.1\r\nHost: x\r\n\r\n{\"a\":1}";
        let end = find_header_end(buf).unwrap();
        assert_eq!(&buf[end..], b"{\"a\":1}");
    }

    #[test]
    fn find_header_end_none_without_terminator() {
        assert!(find_header_end(b"POST /hook HTTP/1.1\r\nHost: x\r\n").is_none());
    }

    #[test]
    fn parse_content_length_cases() {
        let head = "POST /hook HTTP/1.1\r\nHost: x\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(head), 42);
        let head = "POST /hook HTTP/1.1\r\ncontent-length: 7\r\n\r\n";
        assert_eq!(parse_content_length(head), 7);
        let head = "GET /hook HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_content_length(head), 0);
        let head = "POST /hook HTTP/1.1\r\nContent-Length: bogus\r\n\r\n";
        assert_eq!(parse_content_length(head), 0);
    }

    #[test]
    fn http_response_format() {
        let body = "{\"response\":\"hi\"}";
        let resp = http_response(200, "OK", body);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.contains("Content-Type: application/json\r\n"));
        assert!(resp.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.ends_with(body));
    }

    #[test]
    fn json_ok_response_is_valid_json() {
        let resp = json_ok_response("hello \"world\"");
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["response"], "hello \"world\"");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn json_error_response_status_and_json() {
        let resp = json_error_response(404, "Not Found", "not found");
        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"));
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["error"], "not found");
        assert!(v.get("response").is_none());
    }

    #[test]
    fn hook_request_deser() {
        let r: HookRequest = serde_json::from_str("{\"text\":\"hi\"}").unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(r.session, None);
        let r: HookRequest = serde_json::from_str("{\"text\":\"hi\",\"session\":\"s1\"}").unwrap();
        assert_eq!(r.session.as_deref(), Some("s1"));
    }

    #[test]
    fn last_assistant_text_picks_last() {
        let msgs = vec![
            Message::user("q"),
            Message::assistant("first"),
            Message::assistant("second"),
        ];
        assert_eq!(last_assistant_text(&msgs), "second");
        assert_eq!(last_assistant_text(&[]), "(no assistant reply)");
        assert_eq!(
            last_assistant_text(&[Message::user("q")]),
            "(no assistant reply)"
        );
    }

    #[test]
    fn strip_leading_system_messages_works() {
        let msgs = vec![
            Message::system("sys"),
            Message::system("sys2"),
            Message::user("q"),
            Message::assistant("a"),
        ];
        let out = strip_leading_system_messages(msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, MessageRole::User);
        // No leading system messages: unchanged.
        let msgs = vec![Message::user("q")];
        assert_eq!(strip_leading_system_messages(msgs).len(), 1);
    }

    #[tokio::test]
    async fn read_full_request_reassembles_split_chunks() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let body = "{\"text\":\"hello\"}";
        let head = format!(
            "POST /hook HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let (first, rest) = body.split_at(5);
        tokio::spawn(async move {
            client.write_all(head.as_bytes()).await.unwrap();
            client.write_all(first.as_bytes()).await.unwrap();
            tokio::task::yield_now().await;
            client.write_all(rest.as_bytes()).await.unwrap();
        });
        let raw = read_full_request(&mut server)
            .await
            .unwrap()
            .expect("request");
        let (method, path) = parse_request_line(&raw.head).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/hook");
        assert_eq!(raw.body, body.as_bytes());
    }

    #[tokio::test]
    async fn read_full_request_none_on_empty_connection() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        let raw = read_full_request(&mut server).await.unwrap();
        assert!(raw.is_none());
    }
}
