//! SQL execution seam.
//!
//! The extension cannot execute SQL in-process until the Rust SDK ports
//! `vsql::preview::sql_query`, so v1 runs every tool query through a loopback
//! client connection to the server's own listener, configured by
//! `vsql_mcp.db_url`. All SQL execution goes through [`QueryExecutor`]; the
//! loopback implementation is the only thing that changes when native
//! `sql_query` lands.

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

pub trait QueryExecutor {
    /// Run a read-only statement, returning at most `max_rows` rows as JSON
    /// objects. Sets a read-only session and a statement timeout. Uses the
    /// binary protocol so numeric columns arrive typed, not as strings.
    fn read(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String>;

    /// Run a read-only statement whose single string cell we want (EXPLAIN,
    /// SHOW CREATE) — statements the binary/prepared protocol may reject. Uses
    /// the text protocol, so callers must expect string cells.
    fn read_text(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String>;

    /// Run a write statement, returning the affected-row count.
    fn write(&self, sql: &str, timeout_s: u64) -> Result<u64, String>;

    /// Run a read-only statement with positional parameters (internal
    /// schema-introspection queries), returning all rows.
    fn read_params(&self, sql: &str, params: Vec<MyValue>, timeout_s: u64) -> Result<Rows, String>;
}

/// List every schema name the loopback account can see.
pub fn schema_names(exec: &dyn QueryExecutor, timeout_s: u64) -> Result<Vec<String>, String> {
    let rows = exec.read_params(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
        vec![],
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

    fn read_params(&self, sql: &str, params: Vec<MyValue>, timeout_s: u64) -> Result<Rows, String> {
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
