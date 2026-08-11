# AGENTS.md — vsql_mcp

Guidance for AI coding assistants working in this repository.

## What this is

`vsql_mcp` is a VillageSQL (VEF) extension written in Rust. It exposes the
database as a Model Context Protocol (MCP) server over the Streamable HTTP
transport, served from a background worker inside the server process. It is
built with the [VillageSQL Rust SDK](https://github.com/villagesql/vsql-rust-sdk),
not the C++ SDK.

## Build and test

```bash
export VillageSQL_BUILD_DIR=/path/to/villagesql/build
cargo vsql install          # build + package + install the .veb
cargo vsql test             # run the MTR suite
cargo vsql test --record    # regenerate expected results
```

The server must run with `--vsql_allow_preview_extensions=ON` (the test suite
sets this via `mysql-test/t/mcp_basic-master.opt`).

The `villagesql` dependency is a local path to the Rust SDK checkout, because the
published crate predates the preview capabilities this extension uses. Keep it a
path dependency until a crate version ships those capabilities.

## Layout

| Path | Role |
|---|---|
| `src/lib.rs` | Capability wiring, the background worker, the `info()` VDF, `extension!` registration |
| `src/config.rs` | `vsql_mcp.*` sys vars; `ListenConfig` (bind-time) and `RequestConfig` (per request) |
| `src/status.rs` | `SHOW STATUS` counters via the `status_var` capability |
| `src/httpd.rs` | HTTP transport: listener lifecycle, status codes, auth, Origin, sessions |
| `src/mcp.rs` | JSON-RPC layer: protocol constants, session store, method dispatch |
| `src/tools.rs` | The six MCP tools |
| `src/resources.rs` | The `vsql://` resources |
| `src/guardrails.rs` | Statement classification, schema scoping, table-allowlist checks |
| `src/executor.rs` | `QueryExecutor` trait + the loopback client implementation |
| `mysql-test/` | MTR suite (`t/`, `r/`) |

## Non-obvious rules

- **Never call `SYS_VAR.get()` (i.e. `Config`/`ListenConfig`/`RequestConfig::read`,
  or `config::*` reads) from the `thread_worker` Enable or Disable callback.**
  Those fire inside the server's system-variable critical section; reading a sys
  var there re-enters that subsystem and deadlocks the whole server. The worker
  defers all config reads to the Periodic wakeup (see `PENDING_START` in
  `lib.rs`). This is the single most important invariant in the codebase.
- The worker function is a fixed `fn` pointer with no captured state, so all
  server state legitimately lives in statics (`SERVERS`, `SESSIONS`, the status
  atomics).
- `poll()` collects requests under the `SERVERS` lock, then drops the lock before
  handling them — a tool call can run for up to `query_timeout` seconds and must
  not block `stop()`.
- SQL execution goes through the `QueryExecutor` trait. The loopback client is
  the only implementation today; it is the seam that a native `sql_query` path
  will replace. Keep new SQL behind the trait.
- Status-var names are declared without a component prefix; the server prefixes
  them, so `tool_calls_total` surfaces as `vsql_mcp.tool_calls_total`.
- The `query` tool uses the binary protocol (typed values); `EXPLAIN` and
  `SHOW CREATE` go through `read_text` because the prepared protocol may reject
  them.

## Conventions

- One conceptual change per PR; keep the diff focused.
- Bump `manifest.json` and `Cargo.toml` versions together for any behavior
  change. Versions start at `0.0.1`.
- Any error string, error number, or command output that appears in docs must be
  captured from a real run, never written from memory.
- Extensions target correctness within current VEF capabilities; when something
  cannot be done natively yet, document the gap in the README's Known
  Limitations rather than faking it.
