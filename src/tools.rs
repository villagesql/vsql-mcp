//! The MCP tools vsql-mcp advertises and executes.
//!
//! Every tool call runs through the loopback executor and the guardrails in
//! `guardrails`. Results follow the MCP tool-result shape: a `content` array of
//! text blocks plus an `isError` flag. Structured data is returned as a JSON
//! text block, which is how MCP clients surface tabular results to agents.

use mysql::Value as MyValue;
use serde_json::{json, Value as Json};

use crate::config::RequestConfig;
use crate::executor::{self, QueryExecutor};
use crate::guardrails::{self, StmtKind};
use crate::{mcp, status};

/// The advertised tool list with input schemas. `write` is advertised only when
/// it is usable: a tool an agent can see is one it will plan around, and
/// discovering the refusal by attempting a mutation is a wasted turn.
pub fn list(cfg: &RequestConfig) -> Json {
    let mut listing = tool_definitions();
    if !cfg.allow_write {
        if let Some(tools) = listing["tools"].as_array_mut() {
            tools.retain(|t| t.get("name").and_then(Json::as_str) != Some("write"));
        }
    }
    listing
}

fn tool_definitions() -> Json {
    json!({
        "tools": [
            {
                "name": "list_schemas",
                "description": "List schemas visible to the extension.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "list_tables",
                "description": "List tables and views in a schema, with row estimates.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "schema": { "type": "string" } }
                }
            },
            {
                "name": "describe_table",
                "description": "Columns, types, and keys for a table.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "schema": { "type": "string" },
                        "table": { "type": "string" }
                    },
                    "required": ["table"]
                }
            },
            {
                "name": "query",
                "description": "Run a single read-only SELECT and return JSON rows.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "sql": { "type": "string" } },
                    "required": ["sql"]
                }
            },
            {
                "name": "explain",
                "description": "Return EXPLAIN FORMAT=JSON for a candidate query.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "sql": { "type": "string" } },
                    "required": ["sql"]
                }
            },
            {
                "name": "write",
                "description": "Run one INSERT/UPDATE/DELETE (requires vsql_mcp.allow_write=ON).",
                "inputSchema": {
                    "type": "object",
                    "properties": { "sql": { "type": "string" } },
                    "required": ["sql"]
                }
            }
        ]
    })
}

/// Handle a `tools/call` request. Always counts one tool call; counts one tool
/// error on any failure path.
pub fn call(params: &Json, id: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Json {
    status::inc_tool_calls();
    let name = params.get("name").and_then(Json::as_str).unwrap_or("");
    let args = params.get("arguments").unwrap_or(&Json::Null);

    let outcome = match name {
        "list_schemas" => list_schemas(cfg, exec),
        "list_tables" => list_tables(args, cfg, exec),
        "describe_table" => describe_table(args, cfg, exec),
        "query" => query(args, cfg, exec),
        "explain" => explain(args, cfg, exec),
        "write" => write(args, cfg, exec),
        other => Err(format!("unknown tool: {other}")),
    };

    match outcome {
        Ok(value) => mcp::result(id, tool_ok(&value)),
        Err(msg) => {
            status::inc_tool_errors();
            mcp::result(id, tool_err(&msg))
        }
    }
}

/// A successful tool result: the value serialized as one JSON text block.
fn tool_ok(value: &Json) -> Json {
    json!({
        "content": [ { "type": "text", "text": value.to_string() } ],
        "isError": false
    })
}

/// A failed tool result: the message as a text block with `isError` set.
fn tool_err(message: &str) -> Json {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true
    })
}

fn arg_str<'a>(args: &'a Json, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

/// Read a field from a result row as JSON, defaulting to null when absent.
fn field(row: &Json, key: &str) -> Json {
    row.get(key).cloned().unwrap_or(Json::Null)
}

/// The schema a table-scoped tool should act on: the configured schema wins
/// when set, otherwise the caller's argument.
fn effective_schema(arg: Option<&str>, cfg: &RequestConfig) -> Result<String, String> {
    if !cfg.schema.is_empty() {
        return Ok(cfg.schema.clone());
    }
    arg.filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "no schema configured and none supplied".to_owned())
}

fn list_schemas(cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    if !cfg.schema.is_empty() {
        return Ok(json!({ "schemas": [cfg.schema] }));
    }
    let names = executor::schema_names(exec, cfg.query_timeout)?;
    Ok(json!({ "schemas": names }))
}

fn list_tables(args: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let schema = effective_schema(args.get("schema").and_then(Json::as_str), cfg)?;
    let rows = executor::tables_in_schema(exec, &schema, cfg.query_timeout)?;
    if rows.rows.is_empty() && !executor::schema_exists(exec, &schema, cfg.query_timeout)? {
        return Err(format!("no such schema: {schema}"));
    }
    let tables: Vec<Json> = rows
        .rows
        .iter()
        // A listing narrows rather than fails: an excluded table is simply not
        // offered, so an agent never learns it exists.
        .filter(|r| {
            r.get("TABLE_NAME")
                .and_then(Json::as_str)
                .is_some_and(|t| guardrails::table_allowed(t, &cfg.allowed_tables))
        })
        .map(|r| {
            json!({
                "name": field(r, "TABLE_NAME"),
                "type": field(r, "TABLE_TYPE"),
                "row_estimate": field(r, "TABLE_ROWS")
            })
        })
        .collect();
    Ok(json!({ "schema": schema, "tables": tables }))
}

fn describe_table(args: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let table = arg_str(args, "table")?;
    let schema = effective_schema(args.get("schema").and_then(Json::as_str), cfg)?;
    // The allowlist governs what an agent may learn about, not only what it may
    // read: column names are the discovery half of reaching the data.
    if !guardrails::table_allowed(table, &cfg.allowed_tables) {
        return Err(format!("table '{table}' is not in vsql_mcp.allowed_tables"));
    }
    let rows = exec.read_params(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, COLUMN_COMMENT \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
         ORDER BY ORDINAL_POSITION",
        vec![MyValue::from(schema.clone()), MyValue::from(table.to_owned())],
        cfg.query_timeout,
    )?;
    if rows.rows.is_empty() {
        return Err(format!("table not found: {schema}.{table}"));
    }
    let columns: Vec<Json> = rows
        .rows
        .iter()
        .map(|r| {
            json!({
                "name": field(r, "COLUMN_NAME"),
                "type": field(r, "COLUMN_TYPE"),
                "nullable": r.get("IS_NULLABLE").and_then(Json::as_str) == Some("YES"),
                "key": field(r, "COLUMN_KEY"),
                "default": field(r, "COLUMN_DEFAULT"),
                "comment": field(r, "COLUMN_COMMENT")
            })
        })
        .collect();
    Ok(json!({ "schema": schema, "table": table, "columns": columns }))
}

fn query(args: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let sql = arg_str(args, "sql")?;
    if guardrails::classify(sql) != StmtKind::Read {
        return Err("only a single read-only statement is allowed by the query tool".to_owned());
    }
    check_access(sql, cfg, exec)?;
    let rows = exec.read(sql, cfg.max_rows, cfg.query_timeout)?;
    status::add_rows_returned(rows.rows.len() as i64);
    Ok(json!({
        "columns": rows.columns,
        "rows": rows.rows,
        "row_count": rows.rows.len(),
        "truncated": rows.truncated
    }))
}

fn explain(args: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let sql = arg_str(args, "sql")?;
    // Only plan statements that are otherwise runnable by a tool, and hold them
    // to the same schema and allowlist scope as the query/write tools. A write
    // is planned only when the write tool itself is enabled — with writes off,
    // planning one still discloses the target's structure and row estimates.
    match guardrails::classify(sql) {
        StmtKind::Read => {}
        StmtKind::Write if cfg.allow_write => {}
        StmtKind::Write => {
            return Err("cannot explain a write statement while vsql_mcp.allow_write = OFF".to_owned())
        }
        StmtKind::Disallowed => return Err("cannot explain that statement".to_owned()),
    }
    check_access(sql, cfg, exec)?;
    explain_json(sql, cfg, exec)
}

fn write(args: &Json, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    if !cfg.allow_write {
        return Err("the write tool is disabled (set vsql_mcp.allow_write = ON)".to_owned());
    }
    let sql = arg_str(args, "sql")?;
    if guardrails::classify(sql) != StmtKind::Write {
        return Err("the write tool accepts a single INSERT/UPDATE/DELETE statement".to_owned());
    }
    check_access(sql, cfg, exec)?;
    let affected = exec.write(sql, cfg.query_timeout)?;
    Ok(json!({ "affected_rows": affected }))
}

/// Enforce schema scoping and the table allowlist for a statement that is about
/// to run. Uses EXPLAIN FORMAT=JSON to learn which tables the optimizer will
/// actually touch, so joins and subqueries are covered.
fn check_access(sql: &str, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<(), String> {
    if let Some(target) = guardrails::file_write_target(sql) {
        return Err(format!(
            "{target} writes to the server filesystem and is not allowed"
        ));
    }
    if let Some(bad) = guardrails::schema_violation(sql, &cfg.schema) {
        // Say what to do about it: the loopback session has no default
        // database, so the way out is to qualify with the exposed schema, not
        // to drop the qualifier.
        return Err(format!(
            "schema '{bad}' is outside the exposed schema; only '{}' is \
             available, and table names must be qualified with it",
            cfg.schema
        ));
    }
    if cfg.allowed_tables.is_empty() {
        return Ok(());
    }
    // SHOW and DESCRIBE cannot be planned, so the EXPLAIN below would fail with
    // a parser error. The forms that name one table can be checked directly;
    // the rest would enumerate names the allowlist exists to withhold, so they
    // are refused with the reason and the tools that do the same job.
    if let Some(target) = guardrails::metadata_target(sql) {
        return match target {
            guardrails::MetadataTarget::Table(table) => {
                if guardrails::table_allowed(&table, &cfg.allowed_tables) {
                    Ok(())
                } else {
                    Err(format!("table '{table}' is not in vsql_mcp.allowed_tables"))
                }
            }
            guardrails::MetadataTarget::NotTableScoped => Err(format!(
                "{} cannot be used while vsql_mcp.allowed_tables is set, because \
                 it would list objects the allowlist withholds; use the \
                 list_tables and describe_table tools instead",
                guardrails::leading_verb(sql).to_uppercase()
            )),
        };
    }
    let plan = explain_json(sql, cfg, exec)?;
    let mut tables = guardrails::tables_in_explain(&plan);
    if tables.is_empty() {
        // The optimizer can answer without reading any table — MIN()/MAX() from
        // an index, "Impossible WHERE", LIMIT 0 — and then names none, so an
        // empty list is "the plan cannot say", not "touches nothing". Falling
        // back to the statement text keeps an excluded table excluded; a
        // statement that genuinely references nothing (SELECT 1) still yields
        // an empty list here and is allowed.
        tables = guardrails::table_refs_in_text(sql);
    }
    for table in tables {
        if !guardrails::table_allowed(&table, &cfg.allowed_tables) {
            return Err(format!("table '{table}' is not in vsql_mcp.allowed_tables"));
        }
    }
    Ok(())
}

/// Run EXPLAIN FORMAT=JSON and parse the single JSON cell it returns.
fn explain_json(sql: &str, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Result<Json, String> {
    let rows = exec.read_text(&format!("EXPLAIN FORMAT=JSON {sql}"), 10, cfg.query_timeout)?;
    let cell = rows
        .rows
        .first()
        .and_then(Json::as_object)
        .and_then(|m| m.values().next())
        .and_then(Json::as_str)
        .ok_or_else(|| "EXPLAIN returned no plan".to_owned())?;
    serde_json::from_str(cell).map_err(|e| format!("could not parse EXPLAIN output: {e}"))
}
