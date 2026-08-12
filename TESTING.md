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

`mcp_basic` walks the happy paths. The other five files are the adversarial
layer on top of it, split by domain.

| File | Covers |
|---|---|
| `t/mcp_basic.test` | Full end-to-end: initialize/session handshake, `tools/list`, all six tools, both resource URIs, read-only / `allowed_tables` / `schema` / `max_rows` / `query_timeout` guardrails, bearer auth, Origin and body-size transport rules, `GET`→405, session DELETE lifecycle, and the status counters |
| `t/mcp_security.test` | Statement-classification evasion (comments, version comments, multi-statement, DDL, `HANDLER`/`DO`/`SET`/`CALL`, `INTO OUTFILE`), which layer catches what, schema-scope evasion, table-allowlist evasion, bearer auth, Origin/DNS-rebinding, session forging, both `write` tool states, and that credentials never appear in a response |
| `t/mcp_protocol.test` | Spec revision `2025-06-18`: initialize negotiation, structural validation of every advertised `inputSchema`, `tools/call` argument handling, the `vsql://` resource surface including traversal, JSON-RPC framing errors and their codes, and the transport rules |
| `t/mcp_errors.test` | Engine errors as tool results, the read and write timeout paths, empty states, and every `db_url` misconfiguration |
| `t/mcp_types.test` | Round-trip fidelity for `DECIMAL`, 64-bit integers, `DOUBLE`, temporal types with fractional seconds, `JSON`, `BLOB`/`VARBINARY`/`BIT`/`GEOMETRY`, `ENUM`/`SET`, `utf8mb4`, `NULL`, multi-megabyte cells, and server-side `max_rows` truncation |
| `t/mcp_concurrency.test` | 50-way fan-out with no crossed responses, loopback connection accounting, mixed fast/timing-out load, disabling the listener with a call in flight, 12 enable/disable cycles, which settings apply without a rebind, and exact status-counter deltas |
| `t/mcp_tls.test` | A full MCP session over HTTPS, the guardrails on the TLS listener, both listeners serving at once, and the fail-closed paths when certificate material is missing or unreadable. Generates a self-signed certificate per run under `$MYSQL_TMP_DIR` (needs `openssl` on PATH) and removes it afterwards |
| `t/mcp_client.inc` | The shared Python client every driver imports |
| `t/mcp_env.inc`, `t/mcp_env_cleanup.inc` | The two-schema fixture and its teardown |
| `t/<name>-master.opt` | Starts the MTR server with `--vsql_allow_preview_extensions=ON` |
| `r/*.result` | Expected output (generated with `--record`) |

Run one file with `perl mysql-test-run.pl --suite=<path> mcp_security`.

## Reading the assertions

Each driver prints one line per assertion, `<name>: PASS` or `<name>: FAIL
<detail>`, so the recorded `.result` is stable and a regression shows up as a
one-line diff. Nothing that varies per run — ports, timings, session ids, row
estimates — is ever printed.

An assertion whose name begins with `known_` pins **current** behaviour that is
either a documented Known Limitation or a defect the suite found. It passes
today on purpose. When the underlying behaviour is fixed, that assertion flips
and the test fails, which is the intended signal to update the test and the
finding together. Everything else asserts a property that should always hold.

## Conventions worth keeping

- The listener binds `vsql_mcp.port = 0` and the real port is read back from the
  `vsql_mcp.http_port` status variable, so the suite is safe under `--parallel`.
  Re-read it after any disable/enable cycle: an ephemeral port changes on every
  bind.
- Run Python with `-B`. Importing the shared client otherwise leaves a
  `__pycache__` directory in `$MYSQLTEST_VARDIR/tmp` and check-testcase fails
  the run on the leftover.
- The detached helper in `mcp_concurrency` uses a foreground launcher that
  blocks until the child has a request on the wire, never `--exec ... &` plus a
  polling loop, and its output goes to a log under the test tmp dir that is
  inlined into stderr on failure.
- "No side effect" is asserted by querying the table from MTR afterwards, never
  by reading the tool response.
