//! MCP JSON-RPC layer: protocol constants, session store, and method dispatch.
//!
//! Transport concerns (status codes, headers, auth, Origin) live in `httpd`.
//! This module owns what a valid, authenticated JSON-RPC request means once the
//! body is parsed.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::config::RequestConfig;
use crate::executor::QueryExecutor;
use crate::{resources, status, tools};

/// Protocol version this server implements and advertises.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The only JSON-RPC version MCP uses. Required on every request.
pub const JSONRPC_VERSION: &str = "2.0";

/// Versions accepted in the `MCP-Protocol-Version` header. `2025-03-26` is the
/// value the spec says to assume when the header is absent, so we accept it too.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26"];

/// A session with no activity for `vsql_mcp.session_ttl` is treated as gone.
/// MCP clients are supposed to DELETE on exit but often don't, so this bounds
/// the map. Read through a function rather than held as a constant so the
/// setting takes effect on the next request.
fn session_ttl() -> Duration {
    crate::config::session_ttl()
}

static SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// One process-lifetime seed. Hashing the monotonic counter through it gives
/// ids that are unpredictable to outsiders and distinct per counter value.
static ID_SEED: LazyLock<RandomState> = LazyLock::new(RandomState::new);

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, Instant>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Mint a new session id and record it, evicting any expired sessions first.
pub fn new_session() -> String {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = seeded_hash(n);
    let b = seeded_hash(n ^ 0x9e37_79b9_7f4a_7c15);
    let id = format!("{a:016x}{b:016x}");
    let ttl = session_ttl();
    let mut map = lock_sessions();
    map.retain(|_, created| created.elapsed() < ttl);
    map.insert(id.clone(), Instant::now());
    status::set_sessions_active(map.len() as i64);
    id
}

/// A read-only check: an expired session reads as gone even before eviction.
pub fn session_exists(id: &str) -> bool {
    let ttl = session_ttl();
    lock_sessions()
        .get(id)
        .is_some_and(|created| created.elapsed() < ttl)
}

/// Remove a session. Returns true if it existed.
pub fn terminate_session(id: &str) -> bool {
    let mut map = lock_sessions();
    let existed = map.remove(id).is_some();
    status::set_sessions_active(map.len() as i64);
    existed
}

/// Clear every session (called when the worker stops).
pub fn clear_sessions() {
    let mut map = lock_sessions();
    map.clear();
    status::set_sessions_active(0);
}

fn seeded_hash(input: u64) -> u64 {
    let mut h = ID_SEED.build_hasher();
    h.write_u64(input);
    h.finish()
}

pub fn error(id: &Json, code: i64, message: &str) -> Json {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

pub fn result(id: &Json, value: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": value })
}

/// The `initialize` result: advertised protocol version, capabilities, and
/// server identity.
pub fn initialize_result(id: &Json) -> Json {
    result(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "vsql_mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

/// Route a non-initialize JSON-RPC request to its handler and return a full
/// JSON-RPC response object.
pub fn dispatch(method: &str, params: &Json, id: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Json {
    match method {
        "ping" => result(id, json!({})),
        "tools/list" => result(id, tools::list(cfg)),
        "tools/call" => tools::call(params, id, cfg, exec),
        "resources/list" => result(id, resources::list(cfg, exec)),
        "resources/read" => resources::read(params, id, cfg, exec),
        _ => error(id, -32601, &format!("method not found: {method}")),
    }
}
