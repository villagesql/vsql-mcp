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

/// The statement's leading keyword, lowercased, after leading comments and
/// whitespace are stripped. Empty when the statement starts with something that
/// is not a bare word.
pub fn leading_verb(sql: &str) -> String {
    strip_leading(sql)
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The file-writing target of a `SELECT ... INTO OUTFILE`/`INTO DUMPFILE`, if
/// the statement has one.
///
/// These begin with `SELECT`, so classification reads them as a read, and a
/// read-only transaction does not stop a write to the filesystem. Without this
/// the only thing refusing them is the loopback account lacking `FILE`, which
/// makes a guardrail called read-only depend entirely on a grant.
pub fn file_write_target(sql: &str) -> Option<&'static str> {
    let mut previous_was_into = false;
    for ident in idents_outside_strings(sql) {
        if previous_was_into {
            if ident.eq_ignore_ascii_case("outfile") {
                return Some("INTO OUTFILE");
            }
            if ident.eq_ignore_ascii_case("dumpfile") {
                return Some("INTO DUMPFILE");
            }
        }
        previous_was_into = ident.eq_ignore_ascii_case("into");
    }
    None
}

/// Every bare identifier in the statement, in order, skipping string and
/// backtick literals so a keyword inside a value is never mistaken for one.
fn idents_outside_strings(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
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
        // A comment between two keywords must not read as an identifier
        // separating them, or `INTO/* x */OUTFILE` would slip past.
        let after_gap = skip_gap(sql, i);
        if after_gap != i {
            i = after_gap;
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
            b'`' => {
                let (_, ni) = read_ident(sql, i);
                i = ni;
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let (ident, ni) = read_ident(sql, i);
                i = ni;
                if !ident.is_empty() {
                    out.push(ident.to_owned());
                }
            }
            _ => i += 1,
        }
    }
    out
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
                // MySQL accepts whitespace and comments around the qualifying
                // dot, so `db . t` is the same reference as `db.t`. Skipping
                // them here is what stops that spacing being a way past the
                // schema check.
                let dot = skip_gap(sql, i);
                if bytes.get(dot) == Some(&b'.') {
                    let right_start = skip_gap(sql, dot + 1);
                    let (right, ni2) = read_ident(sql, right_start);
                    if !right.is_empty() {
                        out.push((left.to_ascii_lowercase(), right.to_ascii_lowercase()));
                        i = ni2;
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Advance past whitespace and comments starting at `start`, returning the
/// index of the next byte that is neither. Used wherever MySQL allows a gap
/// that must not change how a reference reads.
fn skip_gap(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if sql[i..].starts_with("/*") {
            match sql[i + 2..].find("*/") {
                Some(end) => i = i + 2 + end + 2,
                None => return bytes.len(),
            }
        } else if sql[i..].starts_with("--") || sql[i..].starts_with('#') {
            match sql[i..].find('\n') {
                Some(end) => i += end + 1,
                None => return bytes.len(),
            }
        } else {
            return i;
        }
    }
}

/// Table names appearing in table position in the statement text: the
/// identifier after `FROM`, `JOIN`, `INTO`, `UPDATE` or `TABLE`, with a
/// `schema.` qualifier dropped so the result matches what `EXPLAIN` reports.
///
/// This is a fallback for the case where the optimizer answers a query without
/// naming a table, so `tables_in_explain` returns nothing and cannot speak for
/// what was touched. It is deliberately eager: over-reporting a name costs a
/// rejection, under-reporting one costs the allowlist.
pub fn table_refs_in_text(sql: &str) -> Vec<String> {
    const TABLE_KEYWORDS: &[&str] = &["from", "join", "into", "update", "table"];

    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut expect_table = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
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
        // A comment between the keyword and the name must not lose the name.
        let after_gap = skip_gap(sql, i);
        if after_gap != i {
            i = after_gap;
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
                let quoted = c == b'`';
                let (ident, ni) = read_ident(sql, i);
                i = ni;
                if !quoted && TABLE_KEYWORDS.iter().any(|k| ident.eq_ignore_ascii_case(k)) {
                    expect_table = true;
                    continue;
                }
                if !expect_table {
                    continue;
                }
                expect_table = false;
                // `schema.table` keeps the right half, matching EXPLAIN.
                let dot = skip_gap(sql, i);
                if bytes.get(dot) == Some(&b'.') {
                    let right_start = skip_gap(sql, dot + 1);
                    let (right, ni2) = read_ident(sql, right_start);
                    if !right.is_empty() {
                        out.push(right.to_ascii_lowercase());
                        i = ni2;
                        continue;
                    }
                }
                if !ident.is_empty() {
                    out.push(ident.to_ascii_lowercase());
                }
            }
            _ => {
                // `FROM (SELECT ...)` is a derived table, not a name; the inner
                // FROM sets the flag again for the real one.
                expect_table = false;
                i += 1;
            }
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
                // The optimizer names its own materialisation steps here —
                // `<union1,2>`, `<derived2>`, `<subquery3>`. They are never on
                // an allowlist and are not tables; the real tables feeding them
                // appear elsewhere in the same plan and are still collected.
                if !name.starts_with('<') {
                    out.push(name.to_ascii_lowercase());
                }
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
