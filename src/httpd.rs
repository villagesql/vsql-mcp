//! HTTP transport for MCP Streamable HTTP (spec rev 2025-06-18).
//!
//! Owns listener lifecycle and every transport-level decision — status codes,
//! headers, Origin validation, bearer auth, protocol-version negotiation, and
//! session routing. JSON-RPC method handling lives in `mcp`.

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value as Json;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::config::{self, ListenConfig, RequestConfig};
use crate::executor::Loopback;
use crate::{mcp, status};

/// Largest request body accepted, so a client cannot stream an unbounded body
/// into memory.
const MAX_BODY_BYTES: u64 = 1 << 20;

/// How long the worker will wait for a request body before abandoning the read.
/// tiny_http hands over a request with headers parsed but the body unread, and
/// that read happens on the worker thread; on loopback a 1 MiB body arrives in
/// milliseconds, so this only ever fires on a client that has stopped sending.
const BODY_READ_TIMEOUT_S: u64 = 30;

/// Live listeners while the server is enabled. Taken and dropped on stop.
static SERVERS: Mutex<Option<Vec<Server>>> = Mutex::new(None);

/// Bind the plain HTTP listener and, when TLS material is configured, the HTTPS
/// listener. Idempotent-ish: any previously held servers are dropped first.
///
/// Returns whether the HTTP listener bound. A false return means nothing is
/// serving — the caller reflects that in `enabled` rather than reporting a
/// server that isn't there.
pub fn start(cfg: &ListenConfig) -> bool {
    stop();
    let mut servers = Vec::new();
    let mut http_bound = false;

    // port 0 asks the OS to assign one; a negative port cannot occur (the sys
    // var min is 0), so an unconditional bind is correct.
    match Server::http(format!("127.0.0.1:{}", cfg.port)) {
        Ok(s) => {
            status::set_http_port(bound_port(&s));
            servers.push(s);
            http_bound = true;
        }
        Err(e) => log(&format!("failed to bind HTTP port {}: {e}", cfg.port)),
    }

    // TLS is configured by having certificate material, not by the port number.
    // That leaves `ssl_port = 0` free to mean OS-assigned, as it does for the
    // plain listener — without which there is no way to bind an ephemeral TLS
    // port, and no way to test this listener under parallel workers.
    if !cfg.ssl_cert.is_empty() && !cfg.ssl_key.is_empty() {
        match tls_server(cfg) {
            Ok(s) => {
                status::set_https_port(bound_port(&s));
                servers.push(s);
            }
            Err(e) => log(&format!("failed to bind HTTPS port {}: {e}", cfg.ssl_port)),
        }
    }

    *SERVERS.lock().unwrap_or_else(|e| e.into_inner()) = Some(servers);
    http_bound
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
    // The listener binds happily on a certificate and key that do not belong
    // together, and then fails every handshake — publishing a port that reads
    // as healthy and serves nothing. Checking first turns that into the same
    // visible failure as an unreadable file.
    if let Some(reason) = tls_material_mismatch(&certificate, &private_key) {
        return Err(reason);
    }
    let config = tiny_http::SslConfig {
        certificate,
        private_key,
    };
    Server::https(format!("127.0.0.1:{}", cfg.ssl_port), config).map_err(|e| e.to_string())
}

/// The reason the certificate and key do not belong together, when that can be
/// established. `None` means either they correspond or this check cannot tell.
///
/// Only a PROVEN mismatch refuses the listener. Anything this cannot parse is
/// passed through to `tiny_http`, which reads the same PEM with the same
/// library: a check that refused material the listener would have accepted
/// would be a worse bug than the one it is here to prevent.
///
/// The test is a signing round trip rather than a comparison of key fields. It
/// needs no ASN.1 parsing of our own and it fails for precisely the reason a
/// handshake would.
fn tls_material_mismatch(cert_pem: &[u8], key_pem: &[u8]) -> Option<String> {
    use rustls::SignatureScheme;

    let cert_der = first_certificate(cert_pem)?;
    let key_der = first_private_key(key_pem)?;
    let signing_key = rustls::sign::any_supported_type(&rustls::PrivateKey(key_der)).ok()?;
    let cert = webpki::EndEntityCert::try_from(cert_der.as_slice()).ok()?;

    // Each scheme paired with the webpki algorithm that verifies it, so the
    // signature is checked with the algorithm that produced it.
    let candidates: &[(SignatureScheme, &webpki::SignatureAlgorithm)] = &[
        (SignatureScheme::ECDSA_NISTP256_SHA256, &webpki::ECDSA_P256_SHA256),
        (SignatureScheme::ECDSA_NISTP384_SHA384, &webpki::ECDSA_P384_SHA384),
        (SignatureScheme::ED25519, &webpki::ED25519),
        (SignatureScheme::RSA_PKCS1_SHA256, &webpki::RSA_PKCS1_2048_8192_SHA256),
        (SignatureScheme::RSA_PKCS1_SHA384, &webpki::RSA_PKCS1_2048_8192_SHA384),
        (SignatureScheme::RSA_PKCS1_SHA512, &webpki::RSA_PKCS1_2048_8192_SHA512),
    ];
    const PROBE: &[u8] = b"vsql_mcp tls material check";

    for (scheme, algorithm) in candidates {
        let Some(signer) = signing_key.choose_scheme(&[*scheme]) else {
            continue;
        };
        let Ok(signature) = signer.sign(PROBE) else {
            continue;
        };
        return match cert.verify_signature(algorithm, PROBE, &signature) {
            Ok(()) => None,
            Err(_) => Some(
                "ssl_key does not match ssl_cert: the certificate attests to a \
                 different public key, so every TLS handshake would fail"
                    .to_owned(),
            ),
        };
    }
    // No scheme in common to test with — not a verdict.
    None
}

fn first_certificate(pem: &[u8]) -> Option<Vec<u8>> {
    let mut reader = std::io::BufReader::new(pem);
    rustls_pemfile::certs(&mut reader).ok()?.into_iter().next()
}

fn first_private_key(pem: &[u8]) -> Option<Vec<u8>> {
    // PKCS#8 first, then PKCS#1, which is what this PEM library recognises and
    // therefore all the listener itself can use.
    for parse in [
        rustls_pemfile::pkcs8_private_keys
            as fn(&mut dyn std::io::BufRead) -> std::io::Result<Vec<Vec<u8>>>,
        rustls_pemfile::rsa_private_keys,
    ] {
        let mut reader = std::io::BufReader::new(pem);
        if let Ok(mut keys) = parse(&mut reader) {
            if !keys.is_empty() {
                return Some(keys.remove(0));
            }
        }
    }
    None
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
/// released. Disable runs `stop()` on this same worker thread after the poll
/// loop returns, and the server's `worker_stop()` blocks the thread that ran
/// `SET GLOBAL ... = OFF` in `thread.join()` until it does — so anything that
/// blocks here (a long tool call, or a stalled body read) delays disable and
/// shutdown by exactly that long. The body read is bounded for this reason (see
/// `read_body`); holding the lock only to collect keeps it off the critical path.
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
            // Drop any `userinfo@` prefix: `http://127.0.0.1:9999@evil.com` has
            // authority `127.0.0.1:9999@evil.com`, whose real host is `evil.com`.
            // A browser never sends this (RFC 6454 Origin has no userinfo), but a
            // `:port` split on the raw authority would read the host as local.
            let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
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

/// Compare two secrets without returning early on the first differing byte.
/// The lengths are still distinguishable, which is inherent to comparing
/// without hashing and is not what an attacker is fishing for here.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn auth_ok(req: &Request, require_auth: bool, token: &str) -> bool {
    if !require_auth {
        return true;
    }
    // Requiring auth against an unset token would let `Authorization: Bearer `
    // through, which is the opposite of what the setting asks for.
    if token.is_empty() {
        return false;
    }
    let Some(value) = header(req, "Authorization") else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    // RFC 7235 defines the scheme as case-insensitive; the credential is not.
    scheme.eq_ignore_ascii_case("Bearer") && secret_eq(credential, token)
}

fn protocol_ok(req: &Request) -> bool {
    match header(req, "MCP-Protocol-Version") {
        None => true, // spec: assume 2025-03-26
        Some(v) => mcp::SUPPORTED_VERSIONS.contains(&v),
    }
}

fn handle(request: Request) {
    let path_ok = matches!(
        request.url().split('?').next(),
        Some("/mcp") | Some("/mcp/") | Some("/")
    );
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

fn handle_post(request: Request) {
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

    // Read the (bounded) body without letting a slow client wedge the worker.
    let (request, body) = match read_body(request) {
        BodyRead::Ok(request, body) => (request, body),
        BodyRead::ReadError(request) => {
            respond_json(request, 400, &mcp::error(&Json::Null, -32700, "could not read request body"));
            return;
        }
        // The reader stalled and was abandoned to its helper thread, which owns
        // the request and drops it (closing the socket) once the read finally
        // ends. There is nothing to respond on here; the worker moves on.
        BodyRead::TimedOut => return,
    };
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

/// The outcome of a bounded body read.
enum BodyRead {
    /// The body was read; the request is returned to respond on.
    Ok(Request, String),
    /// The read failed (client hung up mid-body, encoding error); the request
    /// is returned so a 400 can still be sent.
    ReadError(Request),
    /// The read did not finish within `BODY_READ_TIMEOUT_S`. The request has
    /// been left with the helper thread and cannot be responded on here.
    TimedOut,
}

/// Read the request body on a helper thread so a client that sends a short body
/// and then stalls cannot block the worker's poll loop — which would in turn
/// hang disable and shutdown (see `poll`). The read is bounded to `MAX_BODY_BYTES
/// + 1` for the size check, and the worker waits at most `BODY_READ_TIMEOUT_S`.
fn read_body(request: Request) -> BodyRead {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut request = request;
        let mut body = String::new();
        let ok = request
            .as_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_string(&mut body)
            .is_ok();
        // The receiver is gone on timeout; the send simply fails and the request
        // drops here, closing the socket.
        let _ = tx.send((request, body, ok));
    });
    match rx.recv_timeout(Duration::from_secs(BODY_READ_TIMEOUT_S)) {
        Ok((request, body, true)) => BodyRead::Ok(request, body),
        Ok((request, _, false)) => BodyRead::ReadError(request),
        Err(_) => BodyRead::TimedOut,
    }
}

fn log(msg: &str) {
    eprintln!("[vsql_mcp] {msg}");
}
