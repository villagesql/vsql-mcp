// vsql-mcp — expose a VillageSQL database as a Model Context Protocol server.
// Copyright (C) 2026 VillageSQL. Licensed under GPL-2.0.

//! Entry point: wires the three preview capabilities (thread_worker, sys_var,
//! status_var) to the MCP HTTP server and registers the `info()` VDF.
//!
//! The background worker owns the listener lifecycle. On enable it binds the
//! configured ports; on each periodic wakeup it drains pending HTTP requests;
//! on disable it drops the listener and clears sessions.

mod config;
mod executor;
mod ext_tools;
mod guardrails;
mod httpd;
mod mcp;
mod resources;
mod status;
mod tools;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use villagesql::preview::thread_worker::{
    NextWakeup, ThreadHandle, ThreadWorkerCapability, WakeupReason,
};
use villagesql::{InValue, PrerunArgs, PrerunResult, VdfReturn};

/// Reflects whether the server is currently bound, for `info()`.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Set on the Enable wakeup, consumed on the next Periodic wakeup. The Enable
/// callback fires from inside the server's system-variable critical section, so
/// it must NOT read configuration (`SYS_VAR.get`) or bind — doing so re-enters
/// the sys_var subsystem and deadlocks the server. All of that is deferred to
/// the Periodic wakeup, which runs on a plain timer with no lock held.
static PENDING_START: AtomicBool = AtomicBool::new(false);

/// How often the worker wakes to drain pending HTTP requests. Low enough that
/// request latency stays small, high enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The background worker. Runs on the server's worker thread whenever
/// `vsql_mcp.vsql_mcp_enabled` is ON.
fn worker(reason: WakeupReason, _handle: &ThreadHandle) -> NextWakeup {
    match reason {
        WakeupReason::Enable => {
            // Defer the actual bind: see PENDING_START above. No get(), no bind.
            PENDING_START.store(true, Ordering::Relaxed);
        }
        // PollFd is handled but never armed: `NextWakeup::poll_fd` stays 0, so
        // the periodic tick is what drains requests. There is no descriptor
        // worth watching — tiny_http accepts on its own thread and hands over a
        // parsed request through an in-memory queue, so the listening socket's
        // readability says nothing about whether `poll()` has work. The tick is
        // a queue pop every 25ms, which is negligible beside a SQL round trip.
        WakeupReason::Periodic | WakeupReason::PollFd => {
            if PENDING_START.swap(false, Ordering::Relaxed) {
                // Reflect what actually happened: if the HTTP listener did not
                // bind (port in use, say), nothing is serving, so `info()` and
                // the status vars must not report an enabled server. A re-toggle
                // of vsql_mcp_enabled retries; auto-retrying here would spin.
                let bound = httpd::start(config::port());
                ENABLED.store(bound, Ordering::Relaxed);
            }
            httpd::poll();
        }
        WakeupReason::Disable => {
            PENDING_START.store(false, Ordering::Relaxed);
            httpd::stop();
            ENABLED.store(false, Ordering::Relaxed);
        }
    }
    NextWakeup::unchanged()
}

/// The thread_worker capability. Suffix "vsql_mcp" makes the control variable
/// `vsql_mcp.vsql_mcp_enabled`, matching the extension's configuration surface.
static WORKER: ThreadWorkerCapability =
    ThreadWorkerCapability::new(worker, "vsql_mcp", POLL_INTERVAL, None);

/// The `schema` setting as `info_prerun` read it. Each row reports this copy,
/// so a concurrent `SET GLOBAL vsql_mcp.schema` cannot outgrow the buffer the
/// prerun sized from it.
struct InfoState {
    schema: String,
}

/// prerun: size the result buffer from the live `schema` setting. Every other
/// field is a bool, a small int, or a fixed protocol-version string, but
/// `vsql_mcp.schema` is a free-form sys var with no length the SDK bounds (its
/// `SysVarSpec::Str` carries no max), so a fixed `buffer_size` can't cover it.
/// 256 bytes comfortably covers the rest of the JSON (measured fixed overhead
/// is 147 bytes at worst-case field widths); `schema` is added at its
/// JSON-escaped length, which is up to six times its raw length.
fn info_prerun(_args: PrerunArgs, mut out: PrerunResult<InfoState>) {
    let schema = config::schema_setting();
    out.request_buffer_size(256 + serde_json::Value::from(schema.as_str()).to_string().len());
    out.set_state(InfoState { schema });
}

/// SQL: `vsql_mcp.info()` -> STRING (JSON). A liveness probe callable without a
/// database, so tests can assert server state without an HTTP round-trip.
fn info_impl(state: &mut InfoState, _args: &[InValue]) -> VdfReturn {
    let summary = json!({
        "enabled": ENABLED.load(Ordering::Relaxed),
        // True between `vsql_mcp_enabled = ON` and the bind attempt on the next
        // Periodic wakeup, so a caller can tell "not up yet" from "off".
        "starting": PENDING_START.load(Ordering::Relaxed),
        "port": config::port(),
        "http_port": status::http_port(),
        "schema": state.schema,
        "sessions_active": status::sessions_active(),
        "protocol_version": mcp::PROTOCOL_VERSION
    });
    VdfReturn::string(summary.to_string())
}

villagesql::extension! {
    funcs: [
        villagesql::func!(info_impl, "info", [] -> villagesql::Type::String,
            state: InfoState, prerun: info_prerun),
    ],
    requires: [
        &WORKER,
        &config::SYS_VAR,
        &status::STATUS_VAR,
    ]
}
