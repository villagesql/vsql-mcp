// vsql-mcp — expose a VillageSQL database as a Model Context Protocol server.
// Copyright (C) 2026 VillageSQL. Licensed under GPL-2.0.

//! Entry point: wires the three preview capabilities (thread_worker, sys_var,
//! status_var) to the MCP HTTP server and registers the `info()` VDF.
//!
//! The background worker owns the listener lifecycle. On enable it binds the
//! configured ports; on each periodic wakeup it drains pending HTTP requests;
//! on disable it drops the listeners and clears sessions.

mod config;
mod executor;
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
use villagesql::{InValue, VdfReturn};

use config::ListenConfig;

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
fn worker(reason: WakeupReason, _handle: ThreadHandle) -> NextWakeup {
    match reason {
        WakeupReason::Enable => {
            // Defer the actual bind: see PENDING_START above. No get(), no bind.
            PENDING_START.store(true, Ordering::Relaxed);
        }
        WakeupReason::Periodic | WakeupReason::PollFd => {
            if PENDING_START.swap(false, Ordering::Relaxed) {
                let cfg = ListenConfig::read();
                httpd::start(&cfg);
                ENABLED.store(true, Ordering::Relaxed);
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

/// SQL: `vsql_mcp.info()` -> STRING (JSON). A liveness probe callable without a
/// database, so tests can assert server state without an HTTP round-trip.
fn info_impl(_args: &[InValue]) -> VdfReturn {
    let (port, ssl_port) = config::port_settings();
    let summary = json!({
        "enabled": ENABLED.load(Ordering::Relaxed),
        "port": port,
        "ssl_port": ssl_port,
        "http_port": status::http_port(),
        "https_port": status::https_port(),
        "schema": config::schema_setting(),
        "sessions_active": status::sessions_active(),
        "protocol_version": mcp::PROTOCOL_VERSION
    });
    VdfReturn::string(summary.to_string())
}

villagesql::extension! {
    funcs: [
        villagesql::func!(info_impl, "info", [] -> villagesql::Type::String, buffer_size: 512),
    ],
    requires: [
        &WORKER,
        &config::SYS_VAR,
        &status::STATUS_VAR,
    ]
}
