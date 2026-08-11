# Testing vsql_mcp

The suite drives the running MCP server with a small Python client and asserts
the protocol, the six tools, the guardrails, auth, session lifecycle, the
transport rules, and the status counters.

## Prerequisites

- A VillageSQL build, with `VillageSQL_BUILD_DIR` pointing at it.
- The server (started by MTR) must allow preview extensions. The suite sets this
  itself via `mysql-test/t/mcp_basic-master.opt`
  (`--vsql_allow_preview_extensions=ON`).
- `python3` on PATH (standard library only — the test client uses `urllib`).
- The `vsql_mcp.veb` installed into the build's VEB directory
  (`cargo vsql install`).

## Build and install

```bash
export VillageSQL_BUILD_DIR=/path/to/villagesql/build
cargo vsql install
```

## Run the suite

```bash
cargo vsql test
```

Or directly with MTR:

```bash
cd "$VillageSQL_BUILD_DIR/mysql-test"
perl mysql-test-run.pl --suite=/path/to/vsql-mcp/mysql-test mcp_basic
```

## Regenerate expected output

After changing what a test asserts:

```bash
cargo vsql test --record
# or: perl mysql-test-run.pl --suite=/path/to/vsql-mcp/mysql-test --record mcp_basic
```

## How it works

The listener binds an OS-assigned port (`vsql_mcp.port = 0`) so the suite is
safe under `--parallel`; the test reads the actual port back from the
`vsql_mcp.http_port` status variable after waiting for the worker to bind. A
dedicated `mcpuser@'127.0.0.1'` account is the loopback identity, so the
guardrails — not privilege errors — are what reject out-of-scope tables. The
Python client is written into `$MYSQLTEST_VARDIR/tmp` at run time and removed at
cleanup; it prints one deterministic `check: PASS` line per assertion.

## Test files

| File | Covers |
|---|---|
| `mysql-test/t/mcp_basic.test` | Full end-to-end: initialize/session handshake, `tools/list`, all six tools, both resource URIs, read-only / `allowed_tables` / `schema` / `max_rows` / `query_timeout` guardrails, bearer auth, Origin and body-size transport rules, `GET`→405, session DELETE lifecycle, and the status counters |
| `mysql-test/t/mcp_basic-master.opt` | Starts the MTR server with `--vsql_allow_preview_extensions=ON` |
| `mysql-test/r/mcp_basic.result` | Expected output (generated with `--record`) |
