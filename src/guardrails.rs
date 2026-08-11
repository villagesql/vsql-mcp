//! Statement-level safety checks applied before any tool query runs.
//!
//! These are defense in depth, not the only line: the loopback executor also
//! opens a read-only session for the read tools. The checks here give a clear,
//! MCP-level rejection before a statement reaches the server.

use serde_json::Value as Json;

/// What a submitted statement is allowed to do.
#[derive(Debug, PartialEq, Eq)]
pub enum StmtKind {
    Read,
    Write,
    /// DDL, multi-statement, or anything not on either allowlist.
    Disallowed,
}

const READ_VERBS: &[&str] = &["select", "show", "describe", "desc", "explain", "with"];
const WRITE_VERBS: &[&str] = &["insert", "update", "delete", "replace"];

/// Classify a single SQL statement by its leading keyword, after stripping
/// leading comments and whitespace. A trailing second statement (a `;` with
/// non-whitespace after it) forces `Disallowed` — one statement per tool call.
pub fn classify(sql: &str) -> StmtKind {
    let stripped = strip_leading(sql);
    let first = stripped
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("");

    if has_trailing_statement(stripped) {
        return StmtKind::Disallowed;
    }
    if READ_VERBS.iter().any(|v| first.eq_ignore_ascii_case(v)) {
        StmtKind::Read
    } else if WRITE_VERBS.iter().any(|v| first.eq_ignore_ascii_case(v)) {
        StmtKind::Write
    } else {
        StmtKind::Disallowed
    }
}

/// Strip leading `--`, `#`, and `/* */` comments and whitespace.
fn strip_leading(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |x| x.1).trim_start();
        } else if let Some(rest) = s.strip_prefix('#') {
            s = rest.split_once('\n').map_or("", |x| x.1).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |x| x.1).trim_start();
        } else {
            return s;
        }
    }
}

/// True if a `;` is followed by further non-whitespace, non-comment content —
/// i.e. a second statement. A trailing `;` alone is fine.
fn has_trailing_statement(sql: &str) -> bool {
    let mut chars = sql.char_indices();
    let bytes = sql.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_back = false;
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' if !in_double && !in_back => {
                // Skip an escaped quote inside a single-quoted string.
                if in_single && bytes.get(i + 1) == Some(&b'\'') {
                    chars.next();
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single && !in_back => in_double = !in_double,
            '`' if !in_single && !in_double => in_back = !in_back,
            ';' if !in_single && !in_double && !in_back => {
                let rest = sql[i + 1..].trim_start();
                let rest = strip_leading(rest);
                return !rest.is_empty();
            }
            _ => {}
        }
    }
    false
}

/// When a schema is configured, reject any `schema.` qualifier that names a
/// different schema. Table names may be unqualified (resolved in the configured
/// schema) or qualified with the configured schema itself. Qualifiers inside
/// string/backtick literals are ignored.
pub fn schema_violation(sql: &str, configured: &str) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    for (schema, _table) in qualified_refs(sql) {
        if !schema.eq_ignore_ascii_case(configured) {
            return Some(schema);
        }
    }
    None
}

/// Find `<ident>.<ident>` references outside string literals, returning
/// (schema, table) lowercased. Backtick-quoted identifiers are unwrapped so
/// `` `mydb`.`t` `` is recognized. Single-quote handling matches
/// `has_trailing_statement`, including the `''` escape.
fn qualified_refs(sql: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                // A doubled '' is an escaped quote, not the end of the string.
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'`' | b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let (left, ni) = read_ident(sql, i);
                i = ni;
                if bytes.get(i) == Some(&b'.') {
                    let (right, ni2) = read_ident(sql, i + 1);
                    if !right.is_empty() {
                        out.push((left.to_ascii_lowercase(), right.to_ascii_lowercase()));
                    }
                    i = ni2;
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Read one identifier (bare or backtick-quoted) starting at byte `start`,
/// returning the identifier text and the byte index just past it.
fn read_ident(sql: &str, start: usize) -> (&str, usize) {
    let bytes = sql.as_bytes();
    if bytes.get(start) == Some(&b'`') {
        let mut i = start + 1;
        while i < bytes.len() && bytes[i] != b'`' {
            i += 1;
        }
        let text = &sql[start + 1..i];
        // Skip the closing backtick when present.
        return (text, if i < bytes.len() { i + 1 } else { i });
    }
    let mut i = start;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
        i += 1;
    }
    (&sql[start..i], i)
}

/// Given the JSON returned by `EXPLAIN FORMAT=JSON`, collect every
/// `table_name`. Used to enforce `allowed_tables` on whatever the optimizer
/// says the query actually touches — joins and subqueries included.
pub fn tables_in_explain(explain: &Json) -> Vec<String> {
    let mut out = Vec::new();
    walk_tables(explain, &mut out);
    out
}

fn walk_tables(node: &Json, out: &mut Vec<String>) {
    match node {
        Json::Object(map) => {
            if let Some(Json::String(name)) = map.get("table_name") {
                out.push(name.to_ascii_lowercase());
            }
            for v in map.values() {
                walk_tables(v, out);
            }
        }
        Json::Array(items) => {
            for v in items {
                walk_tables(v, out);
            }
        }
        _ => {}
    }
}

/// True if `table` (a bare name from EXPLAIN) is permitted by an allowlist that
/// may hold bare or `schema.table` entries. Matching is on the table name.
pub fn table_allowed(table: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    allowlist.iter().any(|entry| {
        let bare = entry.rsplit('.').next().unwrap_or(entry);
        bare.eq_ignore_ascii_case(table)
    })
}
