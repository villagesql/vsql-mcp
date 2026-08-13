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

/// What a `SHOW` or `DESCRIBE` statement is asking about.
pub enum MetadataTarget {
    /// A specific table, which the allowlist can be applied to.
    Table(String),
    /// Something not scoped to one table — `SHOW TABLES`, `SHOW DATABASES`,
    /// `SHOW STATUS`. There is no single object to check, and the answer would
    /// enumerate names the allowlist exists to withhold.
    NotTableScoped,
}

/// Read the object out of a `SHOW`/`DESCRIBE` statement.
///
/// These cannot be planned with `EXPLAIN`, so the allowlist cannot learn what
/// they read the way it does for a query. The table-scoped forms name their
/// object plainly enough to check directly; the rest do not, and are refused.
///
/// Returns `None` when the statement is not a `SHOW`/`DESCRIBE` at all.
pub fn metadata_target(sql: &str) -> Option<MetadataTarget> {
    let idents = idents_outside_strings(sql);
    let mut it = idents.iter().map(String::as_str);
    let verb = it.next()?;

    // DESCRIBE t / DESC db.t / EXPLAIN t all name the table next.
    if verb.eq_ignore_ascii_case("describe") || verb.eq_ignore_ascii_case("desc") {
        return Some(last_of_qualified(&mut it));
    }
    if !verb.eq_ignore_ascii_case("show") {
        return None;
    }

    match it.next() {
        // SHOW CREATE TABLE t, SHOW CREATE VIEW v
        Some(w) if w.eq_ignore_ascii_case("create") => match it.next() {
            Some(k) if k.eq_ignore_ascii_case("table") || k.eq_ignore_ascii_case("view") => {
                Some(last_of_qualified(&mut it))
            }
            _ => Some(MetadataTarget::NotTableScoped),
        },
        // SHOW COLUMNS FROM t, SHOW INDEX FROM t, and their synonyms. The
        // object follows FROM or IN.
        Some(w)
            if ["columns", "fields", "index", "indexes", "keys"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k)) =>
        {
            for token in it.by_ref() {
                if token.eq_ignore_ascii_case("from") || token.eq_ignore_ascii_case("in") {
                    return Some(last_of_qualified(&mut it));
                }
            }
            Some(MetadataTarget::NotTableScoped)
        }
        _ => Some(MetadataTarget::NotTableScoped),
    }
}

/// Take the next name and keep the table half, matching how the allowlist
/// matches. `idents_outside_strings` emits `db.t` as one token, so the
/// qualifier is still visible here.
fn last_of_qualified<'a>(it: &mut impl Iterator<Item = &'a str>) -> MetadataTarget {
    match it.next() {
        None => MetadataTarget::NotTableScoped,
        Some(name) => MetadataTarget::Table(
            name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase(),
        ),
    }
}

/// Every bare identifier in the statement, in order, skipping string and
/// backtick literals so a keyword inside a value is never mistaken for one.
///
// TODO(villagesql): the quote/backtick/`''`-escape scanning here is hand-rolled
// four times (idents_outside_strings, has_trailing_statement, qualified_refs,
// table_refs). Extract one tokenizer yielding classified spans so a change to
// the escape rules lands in a single place. Kept separate for now to avoid
// reworking security-critical lexing in an unrelated change.
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
                if ident.is_empty() {
                    continue;
                }
                // Emit `db.t` as one token, so a caller can tell a qualifier
                // from the next word in the statement.
                let dot = skip_gap(sql, i);
                if bytes.get(dot) == Some(&b'.') {
                    let right_start = skip_gap(sql, dot + 1);
                    let (right, ni2) = read_ident(sql, right_start);
                    if !right.is_empty() {
                        out.push(format!("{ident}.{right}"));
                        i = ni2;
                        continue;
                    }
                }
                out.push(ident.to_owned());
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
/// different schema. Table names must be qualified with the configured schema:
/// the loopback session has no default database, so an unqualified name does
/// not resolve.
///
/// Not every `x.y` is a schema reference. `p.name` qualifies a column with a
/// table alias and `t1.id` with a table, and reading those as schemas made
/// ordinary aliased SQL unusable whenever this setting was on. Names the
/// statement itself binds — the tables it selects from and the aliases it gives
/// them — are therefore excluded before the comparison.
///
/// The check stays deliberately broad otherwise, so a qualifier that is not a
/// table or alias is still tested. That is what keeps a call into another
/// schema's stored function (`otherdb.some_function()`) from slipping by.
/// Qualifiers inside string and backtick literals are ignored throughout.
pub fn schema_violation(sql: &str, configured: &str) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    let refs = table_refs(sql);

    // A qualifier in table position is a schema, whatever else the statement
    // binds. Testing these first means an alias cannot mask one: without this,
    // `FROM otherdb.t otherdb` binds the very name that qualifies it.
    for r in &refs {
        if let Some(schema) = &r.schema {
            if !schema.eq_ignore_ascii_case(configured) {
                return Some(schema.clone());
            }
        }
    }

    let mut bound: Vec<String> = Vec::new();
    for r in refs {
        bound.push(r.table);
        if let Some(alias) = r.alias {
            bound.push(alias);
        }
    }
    for (schema, _table) in qualified_refs(sql) {
        if bound.iter().any(|b| b.eq_ignore_ascii_case(&schema)) {
            continue;
        }
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
///
/// Works on bytes throughout. The scanners that call this walk unrecognized
/// bytes one at a time, so `start` can land in the middle of a multibyte UTF-8
/// sequence (a Unicode identifier, which MySQL allows); slicing the `str` there
/// would panic, so every comparison here is against the byte slice.
fn skip_gap(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = start;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes[i..].starts_with(b"/*") {
            match find_bytes(&bytes[i + 2..], b"*/") {
                Some(end) => i = i + 2 + end + 2,
                None => return bytes.len(),
            }
        } else if bytes[i..].starts_with(b"--") || bytes.get(i) == Some(&b'#') {
            match bytes[i..].iter().position(|&b| b == b'\n') {
                Some(end) => i += end + 1,
                None => return bytes.len(),
            }
        } else {
            return i;
        }
    }
}

/// Index of the first occurrence of `needle` in `haystack`, by bytes.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A table named in table position, with its `schema.` qualifier and the alias
/// it was given, if it has either.
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
    pub alias: Option<String>,
}

/// The base tables an allowlist should check for `sql`, given its EXPLAIN plan.
///
/// `EXPLAIN FORMAT=JSON` reports the **alias** in `table_name`, not the base
/// table — `FROM orders o` shows `table_name: "o"`. Checking those names
/// directly is wrong in both directions: a legitimate aliased join of allowed
/// tables is rejected (the alias is not on the list), and a forbidden table
/// aliased as an allowed name is admitted (`secret AS customers` shows
/// `customers`). So each EXPLAIN name is mapped back to the base table it
/// aliases, using the alias→table pairs the statement text yields. A name that
/// is not one of the statement's aliases is a real table — an unaliased table,
/// or the underlying table a view exposed to the plan — and is kept as-is. When
/// EXPLAIN named nothing (the optimizer answered without reading a table), the
/// base tables from the text are used instead.
pub fn tables_for_allowlist(sql: &str, explain: &Json) -> Vec<String> {
    let refs = table_refs(sql);
    let alias_to_base: std::collections::HashMap<String, String> = refs
        .iter()
        .filter_map(|r| r.alias.as_ref().map(|a| (a.clone(), r.table.clone())))
        .collect();
    let mut names = tables_in_explain(explain);
    if names.is_empty() {
        names = refs.into_iter().map(|r| r.table).collect();
    }
    names
        .into_iter()
        .map(|n| alias_to_base.get(&n).cloned().unwrap_or(n))
        .collect()
}

/// Tables appearing in table position in the statement text: the identifier
/// after `FROM`, `JOIN`, `INTO`, `UPDATE` or `TABLE`, with a `schema.`
/// qualifier dropped so the result matches what `EXPLAIN` reports, plus the
/// alias bound to it.
///
/// Two callers. The allowlist (via `tables_for_allowlist`) uses the alias→table
/// pairs to map EXPLAIN's alias-named plan entries back to base tables, and the
/// base names as a fallback when the optimizer answers without naming a table.
/// Schema scoping uses the names to tell a `schema.` qualifier apart from an
/// alias or a table qualifying one of its own columns.
///
/// Deliberately eager on the table side: over-reporting a name costs a
/// rejection, under-reporting one costs the allowlist.
pub fn table_refs(sql: &str) -> Vec<TableRef> {
    const TABLE_KEYWORDS: &[&str] = &["from", "join", "into", "update", "table"];
    // Words that can follow a table reference without being an alias. Missing
    // one costs a needless rejection; a schema named after one of these would
    // be the only way to lose a check, which is not a case worth widening for.
    const NOT_AN_ALIAS: &[&str] = &[
        "on", "using", "where", "group", "order", "limit", "having", "join", "inner", "left",
        "right", "full", "cross", "natural", "straight_join", "union", "set", "for", "into",
        "values", "select", "and", "or", "not", "partition", "force", "use", "ignore", "with",
        "window", "procedure", "lock", "offset", "as",
    ];

    let bytes = sql.as_bytes();
    let mut out: Vec<TableRef> = Vec::new();
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
                if ident.is_empty() {
                    continue;
                }
                // `schema.table` keeps the right half, matching EXPLAIN, and
                // remembers the left so scoping can test it.
                let mut schema = None;
                let mut table = ident.to_ascii_lowercase();
                let dot = skip_gap(sql, i);
                if bytes.get(dot) == Some(&b'.') {
                    let right_start = skip_gap(sql, dot + 1);
                    let (right, ni2) = read_ident(sql, right_start);
                    if !right.is_empty() {
                        schema = Some(table);
                        table = right.to_ascii_lowercase();
                        i = ni2;
                    }
                }
                // An alias may follow, with or without AS. A word that cannot
                // be an alias is left unconsumed, because some of them (JOIN)
                // introduce the next table.
                let mut alias = None;
                let peek = skip_gap(sql, i);
                let (next, after_next) = read_ident(sql, peek);
                if next.eq_ignore_ascii_case("as") {
                    let alias_start = skip_gap(sql, after_next);
                    let (named, after_alias) = read_ident(sql, alias_start);
                    if !named.is_empty() {
                        alias = Some(named.to_ascii_lowercase());
                        i = after_alias;
                    }
                } else if !next.is_empty()
                    && !NOT_AN_ALIAS.iter().any(|k| next.eq_ignore_ascii_case(k))
                {
                    alias = Some(next.to_ascii_lowercase());
                    i = after_next;
                }
                out.push(TableRef {
                    schema,
                    table,
                    alias,
                });
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
