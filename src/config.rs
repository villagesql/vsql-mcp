//! Configuration surface: the `vsql_mcp.*` system variables and typed readers.
//!
//! Variables are declared through the `vsql::sys_var` capability and read back
//! live via `SysVarCapability::get`, so `SET GLOBAL vsql_mcp.x = ...` takes
//! effect on the next request with no caching of our own.

use std::ffi::{c_char, c_void, CStr};

use villagesql::preview::sys_var::{SysVarCapability, SysVarSpec};

// The C allocator's free(): releases the string get() hands back in *val.
extern "C" {
    fn free(ptr: *mut c_void);
}

const COMPONENT: &CStr = c"vsql_mcp";

const DEFAULT_PORT: i64 = 3100;
const DEFAULT_SSL_PORT: i64 = 3143;
const DEFAULT_MAX_ROWS: i64 = 1000;
const DEFAULT_QUERY_TIMEOUT: i64 = 30;
const DEFAULT_SCHEMA_TTL: i64 = 60;

/// Every `vsql_mcp.*` variable. `vsql_mcp_enabled` is NOT here — the
/// `thread_worker` capability owns that control variable.
static SPECS: &[SysVarSpec] = &[
    SysVarSpec::Int { name: c"port", comment: c"HTTP listen port (0 = OS-assigned)", default: DEFAULT_PORT, min: 0, max: 65535, on_change: None },
    SysVarSpec::Int { name: c"ssl_port", comment: c"HTTPS listen port (0 = disabled)", default: DEFAULT_SSL_PORT, min: 0, max: 65535, on_change: None },
    SysVarSpec::Str { name: c"ssl_cert", comment: c"Path to the TLS certificate (PEM)", default: c"", on_change: None },
    SysVarSpec::Str { name: c"ssl_key", comment: c"Path to the TLS private key (PEM)", default: c"", on_change: None },
    SysVarSpec::Str { name: c"schema", comment: c"Schema to expose (empty = all non-system schemas)", default: c"", on_change: None },
    SysVarSpec::Bool { name: c"require_auth", comment: c"Require a bearer token on every request", default: false, on_change: None },
    SysVarSpec::Str { name: c"bearer_token", comment: c"Static bearer token when require_auth is ON", default: c"", on_change: None },
    SysVarSpec::Bool { name: c"allow_write", comment: c"Enable the write tool (INSERT/UPDATE/DELETE)", default: false, on_change: None },
    SysVarSpec::Str { name: c"allowed_tables", comment: c"Comma-separated table allowlist; empty = all", default: c"", on_change: None },
    SysVarSpec::Int { name: c"max_rows", comment: c"Row cap per query result", default: DEFAULT_MAX_ROWS, min: 1, max: 1_000_000, on_change: None },
    SysVarSpec::Int { name: c"query_timeout", comment: c"Per-tool-call statement timeout (seconds)", default: DEFAULT_QUERY_TIMEOUT, min: 1, max: 3600, on_change: None },
    SysVarSpec::Int { name: c"schema_ttl", comment: c"Schema cache TTL (seconds)", default: DEFAULT_SCHEMA_TTL, min: 0, max: 86400, on_change: None },
    SysVarSpec::Str { name: c"db_url", comment: c"Loopback DSN the extension runs tool queries through (mysql://user:pass@host:port)", default: c"", on_change: None },
];

pub static SYS_VAR: SysVarCapability = SysVarCapability::new(SPECS);

/// Read one variable's current value as an owned string. Returns `None` when
/// the capability is unavailable (preview off) or the server reports an error.
fn get_raw(name: &CStr) -> Option<String> {
    let mut val: *mut c_void = std::ptr::null_mut();
    let mut val_len: usize = 0;
    // SAFETY: COMPONENT and name are valid NUL-terminated C strings; val and
    // val_len are valid, writable, and live for the call. On Some(false) the
    // server malloc'd a NUL-terminated string into *val, which we free below.
    let result = unsafe {
        SYS_VAR.get(
            COMPONENT.as_ptr(),
            name.as_ptr(),
            &raw mut val,
            &raw mut val_len,
        )
    };
    match result {
        Some(false) => {
            // SAFETY: on success *val is a valid NUL-terminated C string.
            let s = unsafe { CStr::from_ptr(val.cast::<c_char>()) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: *val came from the server's malloc; release per contract.
            unsafe { free(val) };
            Some(s)
        }
        _ => None,
    }
}

fn get_str(name: &CStr) -> String {
    get_raw(name).unwrap_or_default()
}

fn get_int(name: &CStr, fallback: i64) -> i64 {
    get_raw(name)
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(fallback)
}

fn get_bool(name: &CStr) -> bool {
    // The server renders BOOL sys vars as "ON"/"OFF" (also accepts 1/0).
    match get_raw(name) {
        Some(s) => {
            let t = s.trim();
            t.eq_ignore_ascii_case("on") || t == "1" || t.eq_ignore_ascii_case("true")
        }
        None => false,
    }
}

/// The listener-only settings, read once when the worker binds. Keeping these
/// out of `RequestConfig` means a per-request read doesn't fetch four sys vars
/// it never uses.
pub struct ListenConfig {
    pub port: i64,
    pub ssl_port: i64,
    pub ssl_cert: String,
    pub ssl_key: String,
}

impl ListenConfig {
    pub fn read() -> Self {
        Self {
            port: get_int(c"port", DEFAULT_PORT),
            ssl_port: get_int(c"ssl_port", DEFAULT_SSL_PORT),
            ssl_cert: get_str(c"ssl_cert"),
            ssl_key: get_str(c"ssl_key"),
        }
    }
}

/// The settings a single request needs, read once at the start of handling it.
/// Values are stored in the type each consumer wants, converted here where the
/// sys-var bounds already guarantee the conversion is lossless.
pub struct RequestConfig {
    pub schema: String,
    pub require_auth: bool,
    pub bearer_token: String,
    pub allow_write: bool,
    pub allowed_tables: Vec<String>,
    pub max_rows: usize,
    pub query_timeout: u64,
    pub db_url: String,
}

impl RequestConfig {
    pub fn read() -> Self {
        let allowed = get_str(c"allowed_tables");
        let allowed_tables = allowed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        // Bounds: max_rows >= 1, query_timeout in 1..=3600 (see SPECS), so both
        // conversions cannot fail or wrap.
        Self {
            schema: get_str(c"schema"),
            require_auth: get_bool(c"require_auth"),
            bearer_token: get_str(c"bearer_token"),
            allow_write: get_bool(c"allow_write"),
            allowed_tables,
            max_rows: get_int(c"max_rows", DEFAULT_MAX_ROWS).max(1) as usize,
            query_timeout: get_int(c"query_timeout", DEFAULT_QUERY_TIMEOUT).max(1) as u64,
            db_url: get_str(c"db_url"),
        }
    }
}

/// Read just the two fields the auth check needs, for the request methods that
/// do nothing else with configuration.
pub fn auth_settings() -> (bool, String) {
    (get_bool(c"require_auth"), get_str(c"bearer_token"))
}

/// Individual reads for `info()`, which is not on any hot path.
pub fn schema_setting() -> String {
    get_str(c"schema")
}
pub fn port_settings() -> (i64, i64) {
    (get_int(c"port", DEFAULT_PORT), get_int(c"ssl_port", DEFAULT_SSL_PORT))
}
