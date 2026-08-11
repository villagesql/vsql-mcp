//! Status counters exposed through `SHOW STATUS LIKE 'vsql_mcp%'`.
//!
//! Each counter is an `AtomicI64` the extension owns and the server reads live
//! via the `vsql::status_var` capability. The atomics are private; callers go
//! through the accessor functions so the storage and memory ordering stay in
//! one place.

use std::sync::atomic::{AtomicI64, Ordering};

use villagesql::preview::status_var::{StatusVarCapability, StatusVarSpec};

static TOOL_CALLS_TOTAL: AtomicI64 = AtomicI64::new(0);
static TOOL_ERRORS_TOTAL: AtomicI64 = AtomicI64::new(0);
static SESSIONS_ACTIVE: AtomicI64 = AtomicI64::new(0);
static ROWS_RETURNED_TOTAL: AtomicI64 = AtomicI64::new(0);
static HTTP_PORT: AtomicI64 = AtomicI64::new(0);
static HTTPS_PORT: AtomicI64 = AtomicI64::new(0);

/// Declared to the server, which prefixes each name with the component, so
/// these surface as `vsql_mcp.tool_calls_total` etc. under
/// `SHOW STATUS LIKE 'vsql_mcp%'`.
static SPECS: &[StatusVarSpec] = &[
    StatusVarSpec::Int { name: c"tool_calls_total", value: &TOOL_CALLS_TOTAL },
    StatusVarSpec::Int { name: c"tool_errors_total", value: &TOOL_ERRORS_TOTAL },
    StatusVarSpec::Int { name: c"sessions_active", value: &SESSIONS_ACTIVE },
    StatusVarSpec::Int { name: c"rows_returned_total", value: &ROWS_RETURNED_TOTAL },
    StatusVarSpec::Int { name: c"http_port", value: &HTTP_PORT },
    StatusVarSpec::Int { name: c"https_port", value: &HTTPS_PORT },
];

pub static STATUS_VAR: StatusVarCapability = StatusVarCapability::new(SPECS);

pub fn inc_tool_calls() {
    TOOL_CALLS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_tool_errors() {
    TOOL_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn add_rows_returned(n: i64) {
    ROWS_RETURNED_TOTAL.fetch_add(n, Ordering::Relaxed);
}

pub fn set_sessions_active(n: i64) {
    SESSIONS_ACTIVE.store(n, Ordering::Relaxed);
}

pub fn sessions_active() -> i64 {
    SESSIONS_ACTIVE.load(Ordering::Relaxed)
}

pub fn set_http_port(port: i64) {
    HTTP_PORT.store(port, Ordering::Relaxed);
}

pub fn set_https_port(port: i64) {
    HTTPS_PORT.store(port, Ordering::Relaxed);
}

pub fn http_port() -> i64 {
    HTTP_PORT.load(Ordering::Relaxed)
}

pub fn https_port() -> i64 {
    HTTPS_PORT.load(Ordering::Relaxed)
}
