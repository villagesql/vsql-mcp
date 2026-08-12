//! HTTP transport for MCP Streamable HTTP (spec rev 2025-06-18).
//!
//! Owns listener lifecycle and every transport-level decision — status codes,
//! headers, Origin validation, bearer auth, protocol-version negotiation, and
//! session routing. JSON-RPC method handling lives in `mcp`.

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use serde_json::Value as Json;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::config::{self, ListenConfig, RequestConfig};
use crate::executor::Loopback;
use crate::{mcp, status};

/// Largest request body accepted, so a client cannot stream an unbounded body
/// into memory.
const MAX_BODY_BYTES: u64 = 1 << 20;

/// Live listeners while the server is enabled. Taken and dropped on stop.
static SERVERS: Mutex<Option<Vec<Server>>> = Mutex::new(None);

/// Bind the plain HTTP listener and, when TLS material is configured, the HTTPS
/// listener. Idempotent-ish: any previously held servers are dropped first.
pub fn start(cfg: &ListenConfig) {
    stop();
    let mut servers = Vec::new();

    // port 0 asks the OS to assign one; a negative port cannot occur (the sys
    // var min is 0), so an unconditional bind is correct.
    match Server::http(format!("127.0.0.1:{}", cfg.port)) {
        Ok(s) => {
            status::set_http_port(bound_port(&s));
            servers.push(s);
        }
        Err(e) => log(&format!("failed to bind HTTP port {}: {e}", cfg.port)),
    }

    if cfg.ssl_port > 0 && !cfg.ssl_cert.is_empty() && !cfg.ssl_key.is_empty() {
        match tls_server(cfg) {
            Ok(s) => {
                status::set_https_port(bound_port(&s));
                servers.push(s);
            }
            Err(e) => log(&format!("failed to bind HTTPS port {}: {e}", cfg.ssl_port)),
        }
    }

    *SERVERS.lock().unwrap_or_else(|e| e.into_inner()) = Some(servers);
}

/// The port a bound listener actually got. With `port = 0` the OS assigns one,
/// so this is the only way to learn where the server is listening.
fn bound_port(server: &Server) -> i64 {
    server
        .server_addr()
        .to_ip()
        .map_or(0, |addr| i64::from(addr.port()))
}

fn tls_server(cfg: &ListenConfig) -> Result<Server, String> {
    let certificate = std::fs::read(&cfg.ssl_cert).map_err(|e| format!("read ssl_cert: {e}"))?;
    let private_key = std::fs::read(&cfg.ssl_key).map_err(|e| format!("read ssl_key: {e}"))?;
    let config = tiny_http::SslConfig {
        certificate,
        private_key,
    };
    Server::https(format!("127.0.0.1:{}", cfg.ssl_port), config).map_err(|e| e.to_string())
}

/// Drop all listeners and forget every session.
pub fn stop() {
    if let Some(servers) = SERVERS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        drop(servers);
    }
    status::set_http_port(0);
    status::set_https_port(0);
    mcp::clear_sessions();
}

/// Drain and handle every pending request across all listeners. Called from the
/// worker's periodic wakeup, so it must never block or panic out.
///
/// Requests are collected under the SERVERS lock but handled after it is
/// released: a tool call can run for up to `query_timeout` seconds, and
/// `stop()` (which fires from inside the server's sys-var critical section)
/// must not wait that long for the lock.
pub fn poll() {
    let requests: Vec<Request> = {
        let guard = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(servers) = guard.as_ref() else {
            return;
        };
        let mut pending = Vec::new();
        for server in servers {
            while let Ok(Some(request)) = server.try_recv() {
                pending.push(request);
            }
        }
        pending
    };
    for request in requests {
        // A panic while handling one request must not take down the worker.
        let _ = catch_unwind(AssertUnwindSafe(|| handle(request)));
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    // `HeaderField::equiv` only accepts a &'static str, so compare by hand to
    // allow a runtime header name.
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn ctype_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is valid")
}

fn respond_json(request: Request, status: u16, body: &Json) {
    let response = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(ctype_json());
    let _ = request.respond(response);
}

fn respond_empty(request: Request, status: u16) {
    let _ = request.respond(Response::empty(status));
}

/// True unless the Origin header names a non-local host. Absent Origin is
/// allowed (non-browser MCP clients omit it); present-but-remote is rejected to
/// block DNS-rebinding, as the transport spec requires.
fn origin_ok(req: &Request) -> bool {
    match header(req, "Origin") {
        None => true,
        Some(origin) => {
            // authority = everything after scheme, before any path.
            let authority = origin
                .split_once("://")
                .map_or(origin, |(_, rest)| rest)
                .split('/')
                .next()
                .unwrap_or("");
            // Strip a bracketed IPv6 host first (`[::1]` or `[::1]:port`), then
            // a trailing `:port` on a bare host. Without pulling the brackets
            // off first, the `:port` split would cut inside the address.
            let host = if let Some(rest) = authority.strip_prefix('[') {
                rest.split(']').next().unwrap_or("")
            } else {
                authority.rsplit_once(':').map_or(authority, |(h, _)| h)
            };
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
        }
    }
}

fn auth_ok(req: &Request, require_auth: bool, token: &str) -> bool {
    if !require_auth {
        return true;
    }
    header(req, "Authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == token)
}

fn protocol_ok(req: &Request) -> bool {
    match header(req, "MCP-Protocol-Version") {
        None => true, // spec: assume 2025-03-26
        Some(v) => mcp::SUPPORTED_VERSIONS.contains(&v),
    }
}

fn handle(request: Request) {
    let path_ok = matches!(request.url().split('?').next(), Some("/mcp") | Some("/"));
    if !path_ok {
        respond_empty(request, 404);
        return;
    }

    // Origin first: it defends every method.
    if !origin_ok(&request) {
        respond_empty(request, 403);
        return;
    }

    match request.method() {
        Method::Get => {
            // No SSE stream offered at this endpoint.
            respond_empty(request, 405);
        }
        Method::Delete => {
            // DELETE needs only the auth settings, not the full request config.
            let (require_auth, token) = config::auth_settings();
            if !auth_ok(&request, require_auth, &token) {
                respond_empty(request, 401);
                return;
            }
            match header(&request, "Mcp-Session-Id") {
                Some(id) if mcp::terminate_session(id) => respond_empty(request, 200),
                Some(_) => respond_empty(request, 404),
                None => respond_empty(request, 400),
            }
        }
        Method::Post => handle_post(request),
        _ => respond_empty(request, 405),
    }
}

fn handle_post(mut request: Request) {
    let cfg = RequestConfig::read();
    if !auth_ok(&request, cfg.require_auth, &cfg.bearer_token) {
        respond_empty(request, 401);
        return;
    }
    if !protocol_ok(&request) {
        respond_json(
            request,
            400,
            &mcp::error(&Json::Null, -32600, "unsupported MCP-Protocol-Version"),
        );
        return;
    }

    // Read the (bounded) body first; the mutable borrow ends here, so headers
    // can be read afterwards without cloning them out.
    let mut body = String::new();
    if request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .is_err()
    {
        respond_json(request, 400, &mcp::error(&Json::Null, -32700, "could not read request body"));
        return;
    }
    if body.len() as u64 > MAX_BODY_BYTES {
        respond_empty(request, 413);
        return;
    }
    let msg: Json = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            respond_json(request, 400, &mcp::error(&Json::Null, -32700, &format!("parse error: {e}")));
            return;
        }
    };

    // Revision 2025-06-18 removed JSON-RPC batching. An array body would
    // otherwise fall through to the notification path below — no top-level id,
    // so 202 and no body — leaving a batching client waiting for responses that
    // are never coming.
    if msg.is_array() {
        respond_json(
            request,
            400,
            &mcp::error(
                &Json::Null,
                -32600,
                "JSON-RPC batching was removed in MCP 2025-06-18; send one request per POST",
            ),
        );
        return;
    }
    if !msg.is_object() {
        respond_json(
            request,
            400,
            &mcp::error(&Json::Null, -32600, "request must be a JSON object"),
        );
        return;
    }
    if msg.get("jsonrpc").and_then(Json::as_str) != Some(mcp::JSONRPC_VERSION) {
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        respond_json(
            request,
            400,
            &mcp::error(&id, -32600, "jsonrpc must be \"2.0\""),
        );
        return;
    }

    let method = msg.get("method").and_then(Json::as_str);
    let id = msg.get("id").cloned();
    let params = msg.get("params").unwrap_or(&Json::Null);

    // A message with no id is a notification or response: acknowledge with 202.
    let Some(id) = id else {
        respond_empty(request, 202);
        return;
    };
    let Some(method) = method else {
        respond_json(request, 400, &mcp::error(&id, -32600, "not a request"));
        return;
    };

    if method == "initialize" {
        let session = mcp::new_session();
        let session_header = Header::from_bytes(&b"Mcp-Session-Id"[..], session.as_bytes())
            .expect("session id is ASCII hex");
        let response = Response::from_string(mcp::initialize_result(&id).to_string())
            .with_status_code(200)
            .with_header(ctype_json())
            .with_header(session_header);
        let _ = request.respond(response);
        return;
    }

    // Every other request must carry a live session id.
    match header(&request, "Mcp-Session-Id") {
        None => {
            respond_json(request, 400, &mcp::error(&id, -32600, "missing Mcp-Session-Id header"));
            return;
        }
        Some(sid) if !mcp::session_exists(sid) => {
            respond_empty(request, 404);
            return;
        }
        Some(_) => {}
    }

    let exec = Loopback::new(&cfg.db_url);
    let response = mcp::dispatch(method, params, &id, &cfg, &exec);
    respond_json(request, 200, &response);
}

fn log(msg: &str) {
    eprintln!("[vsql_mcp] {msg}");
}
