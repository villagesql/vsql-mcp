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

use mysql::prelude::{Protocol, Queryable};
use mysql::{Conn, Opts, OptsBuilder, Value as MyValue};
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
        let mut conn = self.connect(timeout_s)?;
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
        let mut conn = self.connect(timeout_s)?;
        conn.query_drop(sql).map_err(map_err)?;
        Ok(conn.affected_rows())
    }

    fn read_params(&self, sql: &str, params: Vec<MyValue>, timeout_s: u64) -> Result<Rows, String> {
        let mut conn = self.read_conn(timeout_s, None)?;
        let result = conn.exec_iter(sql, params).map_err(map_err)?;
        collect(result, usize::MAX)
    }
}

/// Turn a driver error into a message, translating the two timeout signatures
/// (server-side MAX_EXECUTION_TIME = error 3024, and the client read timeout =
/// a would-block/timed-out IO error) into one clear line an agent can act on.
fn map_err(e: mysql::Error) -> String {
    let timed_out = match &e {
        mysql::Error::MySqlError(db) => db.code == 3024,
        mysql::Error::IoError(io) => matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
        _ => false,
    };
    if timed_out {
        return "statement exceeded vsql_mcp.query_timeout".to_owned();
    }
    e.to_string()
}

/// Drain a query result into JSON, capping at `max_rows`. When more rows exist
/// than the cap, `truncated` is set and the surplus is consumed and discarded.
fn collect<T: Protocol>(
    mut result: mysql::QueryResult<'_, '_, '_, T>,
    max_rows: usize,
) -> Result<Rows, String> {
    let columns: Vec<String> = result
        .columns()
        .as_ref()
        .iter()
        .map(|c| c.name_str().into_owned())
        .collect();

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
        rows.push(row_to_json(row, &columns));
    }

    Ok(Rows {
        columns,
        rows,
        truncated,
    })
}

fn row_to_json(row: mysql::Row, columns: &[String]) -> Json {
    let mut obj = Map::with_capacity(columns.len());
    // Consume the row's values instead of cloning each cell.
    for (name, cell) in columns.iter().zip(row.unwrap()) {
        obj.insert(name.clone(), value_to_json(cell));
    }
    Json::Object(obj)
}

fn value_to_json(v: MyValue) -> Json {
    match v {
        MyValue::NULL => Json::Null,
        MyValue::Int(i) => Json::from(i),
        MyValue::UInt(u) => Json::from(u),
        MyValue::Float(f) => Json::from(f),
        MyValue::Double(d) => Json::from(d),
        MyValue::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => Json::String(s),
            // Non-UTF-8 (true binary) — render as a lossless hex string rather
            // than dropping bytes or emitting invalid JSON.
            Err(e) => Json::String(format!("0x{}", hex(e.as_bytes()))),
        },
        // Date and time render as their canonical SQL string form.
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
