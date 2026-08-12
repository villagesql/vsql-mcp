//! MCP resources: schema context clients can pull without a tool round-trip.
//!
//! `vsql://<schema>` returns a schema overview; `vsql://<schema>/<table>`
//! returns that table's `CREATE TABLE` DDL.

use serde_json::{json, Value as Json};

use crate::config::RequestConfig;
use crate::executor::{self, QueryExecutor};
use crate::mcp;

/// Advertise resources: one per exposed schema plus one per table. When no
/// schema is configured, only schema-level resources are listed (enumerating
/// every table in every schema would be unbounded).
///
/// A failed introspection query surfaces as an error rather than an empty list:
/// a broken `db_url` and a server with no schemas must not read the same.
pub fn list(cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let mut resources = Vec::new();
    let schemas: Vec<String> = if cfg.schema.is_empty() {
        executor::schema_names(exec, cfg.query_timeout)?
    } else {
        vec![cfg.schema.clone()]
    };

    for schema in &schemas {
        resources.push(json!({
            "uri": format!("vsql://{schema}"),
            "name": format!("Schema {schema}"),
            "description": format!("Overview of schema {schema}"),
            "mimeType": "text/plain"
        }));
    }

    // When a single schema is exposed, also enumerate its tables.
    if !cfg.schema.is_empty() {
        let rows = executor::tables_in_schema(exec, &cfg.schema, cfg.query_timeout)?;
        for row in &rows.rows {
            if let Some(table) = row.get("TABLE_NAME").and_then(Json::as_str) {
                if !crate::guardrails::table_allowed(table, &cfg.allowed_tables) {
                    continue;
                }
                resources.push(json!({
                    "uri": format!("vsql://{}/{table}", cfg.schema),
                    "name": format!("{}.{table}", cfg.schema),
                    "description": "CREATE TABLE DDL",
                    "mimeType": "text/plain"
                }));
            }
        }
    }

    Ok(json!({ "resources": resources }))
}

/// Handle `resources/read` and return a full JSON-RPC response.
pub fn read(params: &Json, id: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Json {
    let uri = match params.get("uri").and_then(Json::as_str) {
        Some(u) => u,
        None => return mcp::error(id, -32602, "missing required parameter: uri"),
    };
    match read_uri(uri, cfg, exec) {
        Ok(text) => mcp::result(
            id,
            json!({
                "contents": [ { "uri": uri, "mimeType": "text/plain", "text": text } ]
            }),
        ),
        Err(msg) => mcp::error(id, -32602, &msg),
    }
}

fn read_uri(uri: &str, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<String, String> {
    let rest = uri
        .strip_prefix("vsql://")
        .ok_or_else(|| format!("unsupported resource URI: {uri}"))?;
    let mut parts = rest.splitn(2, '/');
    let schema = parts.next().unwrap_or("");
    let table = parts.next();

    if schema.is_empty() {
        return Err("resource URI is missing a schema".to_owned());
    }
    if !cfg.schema.is_empty() && !schema.eq_ignore_ascii_case(&cfg.schema) {
        return Err(format!("schema '{schema}' is outside the exposed schema"));
    }

    match table {
        Some(table) if !table.is_empty() => {
            // A table's DDL is as much a disclosure as its rows, so the
            // allowlist applies here exactly as it does to the query tool.
            if !crate::guardrails::table_allowed(table, &cfg.allowed_tables) {
                return Err(format!("table '{table}' is not in vsql_mcp.allowed_tables"));
            }
            table_ddl(schema, table, cfg, exec)
        }
        _ => schema_overview(schema, cfg, exec),
    }
}

fn table_ddl(schema: &str, table: &str, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<String, String> {
    let sql = format!("SHOW CREATE TABLE {}.{}", quote_ident(schema), quote_ident(table));
    let rows = exec.read_text(&sql, 1, cfg.query_timeout)?;
    rows.rows
        .first()
        .and_then(|r| r.get("Create Table").or_else(|| r.get("Create View")))
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("no DDL for {schema}.{table}"))
}

fn schema_overview(schema: &str, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<String, String> {
    let rows = executor::tables_in_schema(exec, schema, cfg.query_timeout)?;
    // The overview enumerates table names, so it is bound by the same allowlist
    // as list_tables and resources/list: an excluded table is simply not shown,
    // never revealed as existing. Filter through the one shared predicate.
    let visible: Vec<&Json> = rows
        .rows
        .iter()
        .filter(|r| {
            r.get("TABLE_NAME")
                .and_then(Json::as_str)
                .is_some_and(|t| crate::guardrails::table_allowed(t, &cfg.allowed_tables))
        })
        .collect();
    let mut out = format!("Schema: {schema}\nTables:\n");
    if visible.is_empty() {
        // An empty listing and a mistyped name look identical, so a client that
        // got the name wrong would read a plausible "no tables" answer instead
        // of a correction. Only pay for the lookup when there is nothing to
        // report. A schema that exists but shows nothing (all tables excluded,
        // or genuinely empty) reads as "(none)".
        if rows.rows.is_empty() && !executor::schema_exists(exec, schema, cfg.query_timeout)? {
            return Err(format!("no such schema: {schema}"));
        }
        out.push_str("  (none)\n");
    }
    for row in visible {
        let name = row.get("TABLE_NAME").and_then(Json::as_str).unwrap_or("?");
        let kind = row.get("TABLE_TYPE").and_then(Json::as_str).unwrap_or("");
        let est = row.get("TABLE_ROWS").map(|v| v.to_string()).unwrap_or_default();
        out.push_str(&format!("  - {name} ({kind}, ~{est} rows)\n"));
    }
    Ok(out)
}

/// Backtick-quote an identifier, doubling any embedded backtick.
fn quote_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}
