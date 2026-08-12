# Tear down the mcp_env.inc fixture and return every vsql_mcp variable to its
# default, so a following test in the same run starts from a known state.

SET GLOBAL vsql_mcp.vsql_mcp_enabled = OFF;
let $wait_condition=
  SELECT VARIABLE_VALUE = 0 FROM performance_schema.global_status
   WHERE VARIABLE_NAME = 'vsql_mcp.http_port';
--source include/wait_condition.inc

SET GLOBAL vsql_mcp.schema = '';
SET GLOBAL vsql_mcp.allowed_tables = '';
SET GLOBAL vsql_mcp.allow_write = OFF;
SET GLOBAL vsql_mcp.require_auth = OFF;
SET GLOBAL vsql_mcp.bearer_token = '';
SET GLOBAL vsql_mcp.max_rows = 1000;
SET GLOBAL vsql_mcp.query_timeout = 30;
SET GLOBAL vsql_mcp.db_url = '';
SET GLOBAL vsql_mcp.ssl_cert = '';
SET GLOBAL vsql_mcp.ssl_key = '';
SET GLOBAL vsql_mcp.ssl_port = 3143;

DROP VIEW mcpsec.v_secret;
DROP VIEW mcpsec.v_leak;
DROP SCHEMA mcpalt;
DROP SCHEMA mcpsec;
DROP USER mcpuser@'127.0.0.1';
UNINSTALL EXTENSION vsql_mcp;
