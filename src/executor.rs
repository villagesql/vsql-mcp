//! SQL execution seam.
//!
//! Two paths run tool queries, chosen per call by [`Hybrid`]:
//!
//! - Native, in-process, via `vsql::preview::sql_query` (see [`native`]): used
//!   only for `read_params`, the fixed-shape internal introspection queries
//!   whose select-list column order the caller already knows and gives back
//!   as `columns` — `sql_query` never has to name a column for us.
//! - Loopback, over `vsql_mcp.db_url` (see [`Loopback`]): used for everything
//!   else — `read` (the `query` tool's arbitrary SQL), `read_text` (EXPLAIN,
//!   but also `SHOW CREATE TABLE` for the `vsql://<schema>/<table>` resource,
//!   which needs the server's real `Create Table` column name), and `write`.
//!   `sql_query` cannot host any of these: it reports no column names or
//!   types at all (a server-side gap; the command-service callback that has
//!   this metadata discards it — see `villagesql/services/preview/sql_query.cc`),
//!   so it can't reproduce typed or name-keyed output; and a session is only
//!   reachable from the worker thread that opened it, so the KILL-based write
//!   timeout below (the only way to bound a write — `MAX_EXECUTION_TIME` does
//!   not apply to INSERT/UPDATE/DELETE) cannot be sent from a second
//!   connection while that thread is busy running the statement.

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::time::Duration;

use mysql::consts::ColumnType;
use mysql::prelude::{Protocol, Queryable};
use mysql::{Column, Conn, Opts, OptsBuilder, Value as MyValue};
use serde_json::{Map, Value as Json};

pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Json>,
    pub truncated: bool,
}

/// One expected column of a `read_params` call, in select-list order. `numeric`
/// tells the native path (see module docs) to parse the cell and emit a JSON
/// number instead of a string, so a column that was a typed integer over the
/// loopback path — `TABLE_ROWS`, say — renders the same way over either path.
/// The loopback path ignores this: the driver already reports the real type.
#[derive(Clone, Copy)]
pub struct ColumnHint {
    pub name: &'static str,
    pub numeric: bool,
}

/// A text/enum column: rendered as a JSON string (or null), same as the
/// loopback path already renders it.
pub const fn col(name: &'static str) -> ColumnHint {
    ColumnHint { name, numeric: false }
}

/// An integer column: rendered as a JSON number (or null) over the native
/// path, matching what the loopback path's typed driver value already gives.
pub const fn numeric_col(name: &'static str) -> ColumnHint {
    ColumnHint { name, numeric: true }
}

pub trait QueryExecutor {
    /// Run a read-only statement, returning at most `max_rows` rows as JSON
    /// objects. Sets a read-only session and a statement timeout. Uses the
    /// binary protocol so numeric columns arrive typed, not as strings.
    fn read(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String>;

    /// Run a read-only statement whose single string cell we want (EXPLAIN,
    /// SHOW CREATE) — statements the binary/prepared protocol may reject, and
    /// (for SHOW CREATE) whose real column name (`Create Table`) the caller
    /// needs. Uses the text protocol, so callers must expect string cells.
    fn read_text(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String>;

    /// Run a write statement, returning the affected-row count.
    fn write(&self, sql: &str, timeout_s: u64) -> Result<u64, String>;

    /// Run a read-only statement with positional parameters, returning all
    /// rows keyed by `columns` — which the caller must give in the exact
    /// order its `sql`'s select list names them, since the native path (see
    /// module docs) cannot report the server's own names back.
    fn read_params(
        &self,
        sql: &str,
        params: Vec<MyValue>,
        columns: &[ColumnHint],
        timeout_s: u64,
    ) -> Result<Rows, String>;
}

/// List every schema name the account can see.
pub fn schema_names(exec: &dyn QueryExecutor, timeout_s: u64) -> Result<Vec<String>, String> {
    let rows = exec.read_params(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
        vec![],
        &[col("SCHEMA_NAME")],
        timeout_s,
    )?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|r| r.get("SCHEMA_NAME").and_then(Json::as_str).map(str::to_owned))
        .collect())
}

/// Whether a schema exists at all. Used to tell "this schema has no tables"
/// apart from "there is no such schema", which otherwise read identically.
pub fn schema_exists(exec: &dyn QueryExecutor, schema: &str, timeout_s: u64) -> Result<bool, String> {
    let rows = exec.read_params(
        "SELECT 1 AS present FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
        vec![MyValue::from(schema.to_owned())],
        &[numeric_col("present")],
        timeout_s,
    )?;
    Ok(!rows.rows.is_empty())
}

/// List the tables and views in one schema, with row estimates.
pub fn tables_in_schema(
    exec: &dyn QueryExecutor,
    schema: &str,
    timeout_s: u64,
) -> Result<Rows, String> {
    exec.read_params(
        "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
        vec![MyValue::from(schema.to_owned())],
        &[col("TABLE_NAME"), col("TABLE_TYPE"), numeric_col("TABLE_ROWS")],
        timeout_s,
    )
}

/// Loopback executor: opens a fresh connection per call. Connection-per-call
/// keeps every tool invocation independent and re-entrant; the worker handles
/// requests serially, so there is no connection contention to pool around.
pub struct Loopback<'a> {
    url: &'a str,
}

impl<'a> Loopback<'a> {
    pub fn new(url: &'a str) -> Self {
        Self { url }
    }

    fn connect(&self, timeout_s: u64) -> Result<Conn, String> {
        if self.url.trim().is_empty() {
            return Err("vsql_mcp.db_url is not set".to_owned());
        }
        let opts = Opts::from_url(self.url).map_err(|e| format!("invalid db_url: {e}"))?;
        let opts = OptsBuilder::from_opts(opts)
            .read_timeout(Some(Duration::from_secs(timeout_s)))
            .write_timeout(Some(Duration::from_secs(timeout_s)));
        Conn::new(opts).map_err(|e| format!("loopback connect failed: {e}"))
    }

    /// Open a read-only session with a statement timeout and, optionally, a
    /// server-side row cap so a huge result set is never fetched only to be
    /// discarded. `row_cap` of `None` leaves `SQL_SELECT_LIMIT` at its default.
    fn read_conn(&self, timeout_s: u64, row_cap: Option<usize>) -> Result<Conn, String> {
        // Headroom so MAX_EXECUTION_TIME below is the timer that fires.
        let mut conn = self.connect(timeout_s.saturating_add(READ_TIMEOUT_HEADROOM_S))?;
        // Defense in depth beyond the statement allowlist: the session cannot
        // write regardless of what slips through classification.
        conn.query_drop("SET SESSION TRANSACTION READ ONLY")
            .map_err(|e| format!("failed to set read-only session: {e}"))?;
        let ms = timeout_s.saturating_mul(1000);
        let mut setup = format!("SET SESSION MAX_EXECUTION_TIME = {ms}");
        if let Some(cap) = row_cap {
            // One extra row so the caller can still tell it truncated.
            let _ = write!(setup, ", SQL_SELECT_LIMIT = {}", cap.saturating_add(1));
        }
        conn.query_drop(setup)
            .map_err(|e| format!("failed to configure read session: {e}"))?;
        Ok(conn)
    }
}

/// Cancels the watchdog when dropped, so a write that finishes in time is never
/// killed. Holding the sender IS the signal: dropping it wakes the thread.
struct KillWatchdog {
    _cancel: Option<std::sync::mpsc::Sender<()>>,
}

impl Loopback<'_> {
    /// Arrange for this connection's statement to be killed if it outlives
    /// `timeout_s`, and return a guard that calls the whole thing off.
    ///
    /// `KILL QUERY` aborts the running statement and rolls back what it had
    /// done, so a timed-out write does not land — which is the difference
    /// between the caller being told "unknown" and being told "did not run".
    /// A connection can always kill its own threads, so this needs no grant
    /// beyond what `db_url` already has.
    fn spawn_kill_watchdog(&self, conn: &mut Conn, timeout_s: u64) -> KillWatchdog {
        let Ok(Some(connection_id)) = conn.query_first::<u64, _>("SELECT CONNECTION_ID()") else {
            // Without an id there is nothing to kill. The client read timeout
            // still bounds the call, which is the behaviour this replaces.
            return KillWatchdog { _cancel: None };
        };
        let url = self.url.to_owned();
        let (cancel, finished) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            // Timeout means the write is still running; Disconnected means the
            // guard was dropped because it finished.
            if finished.recv_timeout(Duration::from_secs(timeout_s))
                != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                return;
            }
            let Ok(opts) = Opts::from_url(&url) else {
                return;
            };
            let opts = OptsBuilder::from_opts(opts)
                .read_timeout(Some(Duration::from_secs(KILL_TIMEOUT_S)))
                .write_timeout(Some(Duration::from_secs(KILL_TIMEOUT_S)));
            if let Ok(mut killer) = Conn::new(opts) {
                let _ = killer.query_drop(format!("KILL QUERY {connection_id}"));
            }
        });
        KillWatchdog {
            _cancel: Some(cancel),
        }
    }
}

/// How long the watchdog's own connection may take. Short: it exists only to
/// send one statement, and a slow kill helps nobody.
const KILL_TIMEOUT_S: u64 = 10;

impl QueryExecutor for Loopback<'_> {
    fn read(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String> {
        let mut conn = self.read_conn(timeout_s, Some(max_rows))?;
        let result = conn.exec_iter(sql, ()).map_err(map_err)?;
        collect(result, max_rows)
    }

    fn read_text(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String> {
        let mut conn = self.read_conn(timeout_s, None)?;
        let result = conn.query_iter(sql).map_err(map_err)?;
        collect(result, max_rows)
    }

    fn write(&self, sql: &str, timeout_s: u64) -> Result<u64, String> {
        // MAX_EXECUTION_TIME does not apply to INSERT/UPDATE/DELETE, so the
        // only way to bound a write is to kill it. Give the client read timeout
        // headroom so the kill is what ends the statement, and the caller gets
        // a definite answer instead of a dropped socket.
        let mut conn = self.connect(timeout_s.saturating_add(READ_TIMEOUT_HEADROOM_S))?;
        let watchdog = self.spawn_kill_watchdog(&mut conn, timeout_s);
        let outcome = conn.query_drop(sql).map_err(map_write_err);
        drop(watchdog);
        outcome?;
        Ok(conn.affected_rows())
    }

    fn read_params(
        &self,
        sql: &str,
        params: Vec<MyValue>,
        _columns: &[ColumnHint],
        timeout_s: u64,
    ) -> Result<Rows, String> {
        // The driver reports the server's real column names for every row, so
        // the caller-given `columns` (needed by the native path; see module
        // docs) is redundant here and ignored.
        let mut conn = self.read_conn(timeout_s, None)?;
        let result = conn.exec_iter(sql, params).map_err(map_err)?;
        collect(result, usize::MAX)
    }
}

/// Extra seconds the client read timeout gets over the server-side statement
/// timeout, so `MAX_EXECUTION_TIME` is what stops a slow read. Without the gap
/// the two expire together and the client wins, which turns a clean server-side
/// kill into a socket timeout. Reads only: `MAX_EXECUTION_TIME` does not apply
/// to a write, so giving the write path headroom would only delay it.
const READ_TIMEOUT_HEADROOM_S: u64 = 5;

/// The kind of the first `io::Error` in this error's source chain, if any.
/// The driver reports a client read timeout as a `CodecError` wrapping the IO
/// error rather than as `Error::IoError`, so matching the outer variant alone
/// misses it — which is why a timed-out statement used to reach the agent as
/// the driver's internal wording.
fn io_kind_of(e: &mysql::Error) -> Option<ErrorKind> {
    match e {
        mysql::Error::IoError(io) => Some(io.kind()),
        // `mysql::Error` itself does not implement `source()`, so the chain has
        // to be entered at the variant that owns the IO error. `PacketCodecError`
        // does implement it, which is enough to reach the `io::Error` without
        // naming that type here.
        mysql::Error::CodecError(codec) => std::error::Error::source(codec)
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind),
        _ => None,
    }
}

/// True when the statement ran out of time, whichever timer fired: the
/// server-side `MAX_EXECUTION_TIME` (error 3024) or the client read timeout.
fn is_timeout(e: &mysql::Error) -> bool {
    if let mysql::Error::MySqlError(db) = e {
        if db.code == 3024 {
            return true;
        }
    }
    matches!(
        io_kind_of(e),
        Some(ErrorKind::WouldBlock | ErrorKind::TimedOut)
    )
}

/// Render a server error as the engine's own sentence. The driver's Display is
/// a Rust debug wrapper (`MySqlError { ERROR 1146 (42S02): ... }`), and this is
/// text an agent reads and acts on, so the wrapper comes off.
fn describe(e: mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(db) => {
            format!("ERROR {} ({}): {}", db.code, db.state, db.message)
        }
        other => other.to_string(),
    }
}

/// Error mapping for the read path, where a timeout means the statement was
/// stopped.
fn map_err(e: mysql::Error) -> String {
    if is_timeout(&e) {
        return "statement exceeded vsql_mcp.query_timeout".to_owned();
    }
    describe(e)
}

/// Error mapping for the write path. `MAX_EXECUTION_TIME` does not apply to
/// INSERT/UPDATE/DELETE, so a timeout here means the client stopped waiting —
/// not that the statement stopped. Saying so is the difference between an agent
/// retrying safely and an agent double-applying a write that already landed.
fn map_write_err(e: mysql::Error) -> String {
    // The watchdog killed it: the statement was aborted and rolled back, so
    // the caller can say the write did not happen.
    if let mysql::Error::MySqlError(db) = &e {
        if db.code == ER_QUERY_INTERRUPTED {
            return "statement exceeded vsql_mcp.query_timeout and was stopped; \
                    the write was rolled back and did not take effect"
                .to_owned();
        }
    }
    // The client stopped waiting before the kill landed. Rare, and the honest
    // answer here is still that the outcome is unknown.
    if is_timeout(&e) {
        return "timed out waiting for the write after vsql_mcp.query_timeout \
                seconds; the statement may still be running on the server and \
                may still commit, so its outcome is unknown"
            .to_owned();
    }
    describe(e)
}

/// `ER_QUERY_INTERRUPTED` — what a killed statement reports.
const ER_QUERY_INTERRUPTED: u16 = 1317;

/// Drain a query result into JSON, capping at `max_rows`. When more rows exist
/// than the cap, `truncated` is set and the surplus is consumed and discarded.
fn collect<T: Protocol>(
    mut result: mysql::QueryResult<'_, '_, '_, T>,
    max_rows: usize,
) -> Result<Rows, String> {
    // Keep the column metadata, not just the names: how a value should be
    // rendered depends on the column's declared type, and the row itself does
    // not carry it.
    let meta: Vec<Column> = result.columns().as_ref().to_vec();
    let columns: Vec<String> = meta.iter().map(|c| c.name_str().into_owned()).collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    for row in result.by_ref() {
        let row = row.map_err(|e| e.to_string())?;
        if rows.len() >= max_rows {
            truncated = true;
            // Keep draining so the connection is left in a clean state. With a
            // server-side SQL_SELECT_LIMIT this loop sees at most one extra row.
            continue;
        }
        rows.push(row_to_json(row, &columns, &meta));
    }

    Ok(Rows {
        columns,
        rows,
        truncated,
    })
}

fn row_to_json(row: mysql::Row, columns: &[String], meta: &[Column]) -> Json {
    let mut obj = Map::with_capacity(columns.len());
    // Consume the row's values instead of cloning each cell.
    for ((name, cell), col) in columns.iter().zip(row.unwrap()).zip(meta) {
        obj.insert(name.clone(), value_to_json(cell, col));
    }
    Json::Object(obj)
}

/// Whether a column holds bytes rather than text, and so should be hex-encoded
/// whatever those bytes happen to be.
///
/// Deciding this by whether the bytes parse as UTF-8 gets it wrong per row: a
/// BIT or GEOMETRY value made only of bytes that are valid UTF-8 came back as
/// raw control characters while the next row of the same column came back as
/// hex, and nothing in the response said which. The column's declared type is
/// the same for every row, so it is the thing to ask.
fn is_binary_column(col: &Column) -> bool {
    // The binary "character set", which is how MySQL marks a string column as
    // holding bytes.
    const BINARY_CHARSET: u16 = 63;
    match col.column_type() {
        ColumnType::MYSQL_TYPE_BIT | ColumnType::MYSQL_TYPE_GEOMETRY => true,
        // JSON is always text, and carries the binary charset on some server
        // versions, so it has to be excluded before the charset test below.
        ColumnType::MYSQL_TYPE_JSON => false,
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_VARCHAR => col.character_set() == BINARY_CHARSET,
        _ => false,
    }
}

fn value_to_json(v: MyValue, col: &Column) -> Json {
    match v {
        MyValue::NULL => Json::Null,
        MyValue::Int(i) => Json::from(i),
        MyValue::UInt(u) => Json::from(u),
        MyValue::Float(f) => Json::from(f),
        MyValue::Double(d) => Json::from(d),
        MyValue::Bytes(b) => {
            if is_binary_column(col) {
                return Json::String(format!("0x{}", hex(&b)));
            }
            match String::from_utf8(b) {
                Ok(s) => Json::String(s),
                // A text column holding bytes that are not valid UTF-8 would
                // otherwise have to lose them; hex keeps the value whole.
                Err(e) => Json::String(format!("0x{}", hex(e.as_bytes()))),
            }
        }
        // A date-typed value carries a time whether or not it is used, so the
        // driver's rendering drops the time part for any DATETIME at midnight,
        // the zero datetime included. The column type says which it is.
        MyValue::Date(y, m, d, h, min, s, us) => {
            if col.column_type() == ColumnType::MYSQL_TYPE_DATE {
                Json::String(format!("{y:04}-{m:02}-{d:02}"))
            } else if us > 0 {
                Json::String(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{us:06}"))
            } else {
                Json::String(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}"))
            }
        }
        // TIME renders as its canonical SQL string form, negatives included.
        other => Json::String(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The in-process execution path, backed by an open `vsql::preview::sql_query`
/// session. See the module docs for what this can and cannot host.
mod native {
    use serde_json::{Map, Value as Json};
    use villagesql::preview::sql_query::{Diag, Session};

    use super::{ColumnHint, Rows};

    /// `MAX_EXECUTION_TIME` interrupting a statement — the same numeric error
    /// the loopback path already matches on, for the same reason (see
    /// `map_err` above).
    const ER_QUERY_INTERRUPTED: u32 = 3024;

    fn describe(diag: &Diag) -> String {
        format!("ERROR {} ({}): {}", diag.errno, diag.sqlstate, diag.message)
    }

    fn map_err(diag: Diag) -> String {
        if diag.errno == ER_QUERY_INTERRUPTED {
            return "statement exceeded vsql_mcp.query_timeout".to_owned();
        }
        describe(&diag)
    }

    /// Run one setup statement (session mode, timeout) that must succeed
    /// before the real query runs.
    fn run_admin(session: &Session, sql: &str) -> Result<(), String> {
        let result = session
            .execute(sql)
            .ok_or_else(|| "sql_query: could not open a session-setup statement".to_owned())?;
        match result.error() {
            Some(diag) => Err(describe(&diag)),
            None => Ok(()),
        }
    }

    /// Put the session into the same read-only, timeout-bounded state the
    /// loopback path sets up per connection (see `Loopback::read_conn`).
    fn set_read_only(session: &Session, timeout_s: u64) -> Result<(), String> {
        run_admin(session, "SET SESSION TRANSACTION READ ONLY")?;
        run_admin(
            session,
            &format!("SET SESSION MAX_EXECUTION_TIME = {}", timeout_s.saturating_mul(1000)),
        )
    }

    /// Substitute each `?` in `sql` with `params[i]` rendered as an escaped SQL
    /// literal. `sql_query` takes only raw text (villagesql-server#627 tracks
    /// giving it real bind values); this is the one place in the extension
    /// that has to do the escaping by hand, and it does so with the same
    /// driver (`mysql::Value::as_sql`) the loopback path already trusts to
    /// quote a literal correctly.
    fn bind(sql: &str, params: &[super::MyValue]) -> Result<String, String> {
        let mut parts = sql.split('?');
        let mut out = parts
            .next()
            .ok_or_else(|| "empty statement".to_owned())?
            .to_owned();
        for (part, param) in parts.zip(params) {
            out.push_str(&param.as_sql(true));
            out.push_str(part);
        }
        // One fewer `?` than params, or vice versa, means the caller's
        // placeholder count doesn't match — a bug in this extension's own
        // code, not something a live server round trip would ever surface.
        if sql.matches('?').count() != params.len() {
            return Err(format!(
                "sql_query: {} placeholder(s) in statement but {} parameter(s) given",
                sql.matches('?').count(),
                params.len()
            ));
        }
        Ok(out)
    }

    /// Run a statement with positional parameters, keying every row with
    /// `columns` in position order (see `QueryExecutor::read_params`).
    pub fn read_params(
        session: &Session,
        sql: &str,
        params: Vec<super::MyValue>,
        columns: &[ColumnHint],
        timeout_s: u64,
    ) -> Result<Rows, String> {
        let bound = bind(sql, &params)?;
        set_read_only(session, timeout_s)?;
        let mut result = session
            .execute(&bound)
            .ok_or_else(|| "sql_query: could not open the statement".to_owned())?;
        if let Some(diag) = result.error() {
            return Err(map_err(diag));
        }
        let mut rows = Vec::new();
        while let Some(row) = result.next_row() {
            let mut obj = Map::with_capacity(columns.len());
            for (i, hint) in columns.iter().enumerate() {
                let cell = row.get_str(i as u32);
                let value = match (cell, hint.numeric) {
                    (None, _) => Json::Null,
                    (Some(s), true) => s
                        .trim()
                        .parse::<i64>()
                        .map_or(Json::Null, Json::from),
                    (Some(s), false) => Json::String(s.to_owned()),
                };
                obj.insert(hint.name.to_owned(), value);
            }
            rows.push(Json::Object(obj));
        }
        Ok(Rows {
            columns: columns.iter().map(|h| h.name.to_owned()).collect(),
            rows,
            truncated: false,
        })
    }
}

/// Routes each `QueryExecutor` method to whichever path can host it — see the
/// module docs. `native` is `None` when the capability is unavailable (preview
/// extensions off, or an SDK build predating `sql_query`), in which case every
/// method falls back to the loopback connection exactly as before.
pub struct Hybrid<'a> {
    loopback: Loopback<'a>,
    // `SQL_QUERY` is a `static`, so every `Session` it opens is `Session<'static>`
    // regardless of how long the borrow held here lasts.
    native: Option<&'a villagesql::preview::sql_query::Session<'static>>,
}

impl<'a> Hybrid<'a> {
    pub fn new(
        db_url: &'a str,
        native: Option<&'a villagesql::preview::sql_query::Session<'static>>,
    ) -> Self {
        Self {
            loopback: Loopback::new(db_url),
            native,
        }
    }
}

impl QueryExecutor for Hybrid<'_> {
    fn read(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String> {
        self.loopback.read(sql, max_rows, timeout_s)
    }

    fn read_text(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String> {
        self.loopback.read_text(sql, max_rows, timeout_s)
    }

    fn write(&self, sql: &str, timeout_s: u64) -> Result<u64, String> {
        self.loopback.write(sql, timeout_s)
    }

    fn read_params(
        &self,
        sql: &str,
        params: Vec<MyValue>,
        columns: &[ColumnHint],
        timeout_s: u64,
    ) -> Result<Rows, String> {
        match self.native {
            Some(session) => native::read_params(session, sql, params, columns, timeout_s),
            None => self.loopback.read_params(sql, params, columns, timeout_s),
        }
    }
}
