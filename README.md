# VillageSQL MCP Extension

`vsql_mcp` exposes a VillageSQL database as a
[Model Context Protocol](https://modelcontextprotocol.io) server, so MCP
clients — Claude Code, Claude Desktop, IDE agents — can discover the schema and
run governed queries without a sidecar process. Where `vsql-rest` turns a schema
into a REST API for programs, `vsql-mcp` turns it into a tool surface for
agents.

It serves MCP over the **Streamable HTTP** transport (spec revision
`2025-06-18`) on a listener owned by a background worker inside the server
process.

```sql
INSTALL EXTENSION vsql_mcp;
SET GLOBAL vsql_mcp.db_url = 'mysql://mcp_user:password@127.0.0.1:3306';
SET GLOBAL vsql_mcp.schema = 'mydb';
SET GLOBAL vsql_mcp.port = 3100;
SET GLOBAL vsql_mcp.vsql_mcp_enabled = ON;
```

```bash
claude mcp add --transport http mydb http://localhost:3100/mcp
```

## Requirements

`vsql_mcp` uses the VEF preview capabilities `thread_worker`, `sys_var`, and
`status_var` (the same set `vsql-rest` uses), so the server must be started with
preview extensions allowed:

```bash
mysqld --vsql_allow_preview_extensions=ON ...
```

`SET PERSIST vsql_allow_preview_extensions = ON` also works and takes effect for
the next server start.

## Building

Requires the [VillageSQL Rust SDK](https://github.com/villagesql/vsql-rust-sdk)
and `cargo-vsql` (`cargo install cargo-vsql`), plus a Rust toolchain 1.87+.

```bash
export VillageSQL_BUILD_DIR=/path/to/villagesql/build
cargo vsql install
```

`cargo vsql install` compiles in release mode, packages `dist/vsql_mcp.veb`, and
copies it into the server's VEB directory.

## Installing

```sql
INSTALL EXTENSION vsql_mcp;
```

The extension registers its configuration and status variables immediately;
nothing listens until you set `vsql_mcp.db_url` and turn
`vsql_mcp.vsql_mcp_enabled` ON.

## How queries run

The extension runs every tool query through a **loopback client connection** to
the server, configured by `vsql_mcp.db_url`. Point that DSN at a dedicated MySQL
account and tool calls run as that account under its real `GRANT`s — the
allowlist and read-only enforcement below are defense in depth on top of the
grants, not a replacement for them.

A native in-process path will replace the loopback connection once the Rust SDK
ports the `sql_query` capability (see [Known Limitations](#known-limitations)).

## Configuration

All variables are `SET GLOBAL vsql_mcp.<name>`.

| Variable | Default | Purpose |
|---|---|---|
| `vsql_mcp_enabled` | OFF | Start/stop the server |
| `port` | 3100 | HTTP listen port (0 = OS-assigned) |
| `ssl_port` | 3143 | HTTPS listen port (0 = OS-assigned) |
| `ssl_cert` / `ssl_key` | `""` | PEM paths; both required to serve HTTPS |
| `schema` | `""` | Schema to expose (empty = all non-system schemas) |
| `require_auth` | OFF | Require a bearer token on every request |
| `bearer_token` | `""` | Static token checked when `require_auth` is ON |
| `allow_write` | OFF | Enable the `write` tool |
| `allowed_tables` | `""` | Comma-separated table allowlist; empty means all |
| `max_rows` | 1000 | Row cap per `query` result |
| `query_timeout` | 30 | Per-tool-call statement timeout (seconds) |
| `session_ttl` | 1800 | Idle MCP session lifetime (seconds) |
| `db_url` | `""` | Loopback DSN tool queries run through |

HTTPS is served when both `ssl_cert` and `ssl_key` are set, and not otherwise —
leaving either empty is how you turn TLS off, whatever `ssl_port` says.

Port and TLS changes take effect when the server is next enabled — toggle
`vsql_mcp_enabled` OFF then ON after changing them.

## Tools

The server advertises six tools via `tools/list`:

| Tool | Purpose |
|---|---|
| `list_schemas` | Schemas visible to the extension |
| `list_tables` | Tables and views in a schema, with row estimates |
| `describe_table` | Columns, types, and keys for a table |
| `query` | Run a single read-only `SELECT` and return JSON rows |
| `explain` | `EXPLAIN FORMAT=JSON` for a candidate query |
| `write` | One `INSERT`/`UPDATE`/`DELETE`/`REPLACE` (requires `allow_write = ON`) |

`write` is advertised in `tools/list` only while `allow_write` is ON, so an
agent never plans around a tool it cannot use.

Tool results follow the MCP shape: a `content` array with a JSON text block,
plus `isError`. A rejected statement returns `isError: true` with a message
naming the guardrail that stopped it.

### Guardrails

Applied to every `query`, `explain`, and `write` call:

- **Read-only enforcement.** `query` accepts a single `SELECT`/`SHOW`/`WITH`/
  `EXPLAIN`/`DESCRIBE`; the loopback session is also set `READ ONLY`. `write`
  accepts a single `INSERT`/`UPDATE`/`DELETE`/`REPLACE` and only when
  `allow_write` is ON. A trailing second statement is rejected.
- **No row ceiling on writes.** `max_rows` caps what a `query` returns; nothing
  caps what a `write` changes. An unqualified `DELETE` empties the table. Give
  `db_url` an account whose grants match the blast radius you are willing to
  accept.
- **Schema scoping.** With `schema` set, a reference to any other schema is
  rejected. Table names must be qualified with the exposed schema
  (`mydb.orders`): the loopback connection has no default database, so a bare
  name does not resolve. Aliases and table-qualified columns work normally —
  `p.total` is a column, not a schema.

  Scoping reads the statement, so a view inside the exposed schema that selects
  from another one is reachable through it, as SQL intends for a view. Use
  `allowed_tables` when you need confinement that holds through a view: that
  check plans the statement and sees the underlying table.
- **Table allowlist.** With `allowed_tables` set, the statement is planned with
  `EXPLAIN FORMAT=JSON` and every table it touches (joins and subqueries
  included) must be on the list. When the optimizer answers without naming a
  table — `MIN()`/`MAX()` from an index, an impossible `WHERE`, `LIMIT 0` — the
  tables named in the statement text are checked instead, so an elided plan is
  not a way past the list. Matching is on the table name.

  The allowlist also governs what an agent can learn about a table, not only
  what it can read: `describe_table` and `vsql://<schema>/<table>` refuse an
  excluded table, and `list_tables` and `resources/list` omit it.

  `SHOW` and `DESCRIBE` cannot be planned, so their object is read from the
  statement instead. The forms naming one table — `DESCRIBE t`,
  `SHOW CREATE TABLE t`, `SHOW COLUMNS FROM t`, `SHOW INDEX FROM t` — are
  checked against the list like any other reference. The forms that enumerate,
  such as `SHOW TABLES` and `SHOW DATABASES`, are refused while a list is set,
  because their answer is the names the list exists to withhold; the message
  points at `list_tables` and `describe_table`, which filter.
- **No writes to the filesystem.** `SELECT ... INTO OUTFILE` and `INTO DUMPFILE`
  are refused. They begin with `SELECT`, so neither the statement allowlist nor
  the read-only session stops them; without this the only thing that would is
  the `db_url` account lacking `FILE`.
- **Row cap.** `max_rows` caps a `query` result and marks it `truncated`.
- **Statement timeout.** `query_timeout` bounds each call. A read is stopped by
  `MAX_EXECUTION_TIME`; a write, which that setting does not govern, is stopped
  by killing the statement, so it rolls back rather than landing after the call
  has returned. A client read timeout backs both up.

## Resources

Clients can pull schema context without a tool round-trip:

- `vsql://<schema>` — a schema overview (its tables)
- `vsql://<schema>/<table>` — that table's `CREATE TABLE` DDL

## Authentication

Set `require_auth = ON` and `bearer_token` to require
`Authorization: Bearer <token>` on every request; requests without it get
HTTP 401. This is a single static token — for per-user identity, use a dedicated
`db_url` account so MySQL `GRANT`s do the enforcing.

The transport also validates the `Origin` header (a request from a non-local
origin gets HTTP 403), as the MCP Streamable HTTP spec requires.

## Monitoring

`SHOW STATUS LIKE 'vsql_mcp%'` reports:

| Status variable | Meaning |
|---|---|
| `vsql_mcp.tool_calls_total` | Tool calls handled |
| `vsql_mcp.tool_errors_total` | Tool calls that returned an error |
| `vsql_mcp.sessions_active` | Live MCP sessions |
| `vsql_mcp.rows_returned_total` | Rows returned by the `query` tool |
| `vsql_mcp.http_port` / `vsql_mcp.https_port` | Actually-bound ports (0 = not listening) |

`SELECT vsql_mcp.info();` returns the same liveness summary as JSON.

## Known Limitations

- **SQL runs through a loopback connection.** The Rust SDK has not yet ported
  the `sql_query` capability, so tool queries reach the database over a client
  connection configured by `vsql_mcp.db_url` rather than in-process. Set
  `db_url` to a dedicated account. A native path replaces this when `sql_query`
  lands.
- **Requires the SDK as a source dependency.** The published `villagesql` crate
  predates the preview capabilities this extension needs, so it builds against a
  local checkout of the Rust SDK until a crate version ships with them.
- **No SSE / server-initiated messages.** `GET /mcp` returns HTTP 405, which the
  spec allows for servers without a stream; there are no progress notifications.
- **Requests are handled one at a time.** The worker drains and serves requests
  serially; `query_timeout` bounds how long any one call can hold the line. A
  stalled request body is read on a helper thread with a bounded wait, so a slow
  client cannot wedge the worker or block disable/shutdown.
- **The allowlist does not see through a stored function.** `allowed_tables` and
  `schema` are enforced on the tables a statement plans with `EXPLAIN`, which
  does not descend into a function body. A `SELECT some_function()` names no
  table, so it passes both checks even if the function reads an excluded table.
  Likewise a read tool can call any function the `db_url` account may call —
  including one that opens a network connection (`http_get()` and the like) — so
  a read grants the full reach of installed functions, not only table reads.
  When you need hard confinement, put it in the `db_url` account's `GRANT`s, not
  only in `allowed_tables`.
- **Secrets are visible to privileged users.** `bearer_token` and `db_url` (which
  embeds a password) are global variables readable via `SHOW VARIABLES` by users
  with the privilege.
- **A `write` that overruns is killed, not waited out.** `MAX_EXECUTION_TIME`
  applies to `SELECT` only, so `query_timeout` is enforced on a write by issuing
  `KILL QUERY` against its own connection. The statement is aborted and rolled
  back, so a timed-out write does not take effect and the tool says so. If the
  kill itself cannot be delivered the result falls back to reporting the outcome
  as unknown.

## Security Considerations

`vsql_mcp` opens a network listener and executes agent-supplied SQL, so treat it
as a security boundary:

- Bind is `127.0.0.1` only; front it with a reverse proxy if you need remote
  access, and terminate TLS there or via `ssl_cert`/`ssl_key`.
- Give `db_url` a least-privilege account. Read-only is enough unless you enable
  the `write` tool, and the account's `GRANT`s are the real access control.
- Keep `require_auth` ON for anything beyond a local experiment, and prefer a
  proxy that maps identities over the single static token.
- `allowed_tables` and `schema` narrow what an agent can reach even within the
  account's grants.

## Testing

See [TESTING.md](TESTING.md).

## Contributing

See the [VillageSQL Contributing Guide](https://github.com/villagesql/villagesql-server/blob/main/CONTRIBUTING.md).

## Reporting Bugs and Requesting Features

Open an issue on [GitHub](https://github.com/villagesql/vsql-mcp/issues).

## Contact

- Discord: https://discord.gg/KSr6whd3Fr
- GitHub Issues: https://github.com/villagesql/vsql-mcp/issues

## License

GPL-2.0. See [LICENSE](LICENSE).
