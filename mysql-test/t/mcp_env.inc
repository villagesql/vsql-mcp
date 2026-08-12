# Standard fixture for the adversarial vsql_mcp suites.
#
# Builds two schemas so cross-schema and allowlist evasion have something real
# to reach for:
#
#   mcpsec.t1        the in-scope table
#   mcpsec.secret    an out-of-scope table holding a value we assert never leaks;
#                    password_hash is indexed on purpose, because an index is
#                    what lets the optimizer answer MIN()/MAX() without naming
#                    the table in EXPLAIN
#   mcpsec.v_leak    a view inside the exposed schema that reads the other one
#   mcpsec.v_secret  a view inside the exposed schema over a non-allowlisted table
#   mcpalt.t1        same bare table name as mcpsec.t1, in a different schema
#
# The loopback account gets broad SELECT (and SHOW VIEW, so EXPLAIN can see
# through a view) precisely so that the guardrails, not privilege errors, are
# what reject an out-of-scope statement. Writes are granted only on mcpsec.
#
# Leaves $mcp_port set to the actually-bound listener port. The listener binds
# port 0 so the suite is safe under --parallel; never hardcode a port.

INSTALL EXTENSION vsql_mcp;

CREATE USER mcpuser@'127.0.0.1';
GRANT SELECT, SHOW VIEW ON *.* TO mcpuser@'127.0.0.1';

CREATE SCHEMA mcpsec;
GRANT INSERT, UPDATE, DELETE ON mcpsec.* TO mcpuser@'127.0.0.1';
CREATE TABLE mcpsec.t1 (id INT PRIMARY KEY, name VARCHAR(50));
INSERT INTO mcpsec.t1 VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');
CREATE TABLE mcpsec.secret (id INT PRIMARY KEY, password_hash VARCHAR(80), KEY ix_ph (password_hash));
INSERT INTO mcpsec.secret VALUES (1, 'hunter2hash');

CREATE SCHEMA mcpalt;
CREATE TABLE mcpalt.t1 (id INT PRIMARY KEY, tag VARCHAR(30));
INSERT INTO mcpalt.t1 VALUES (1, 'other-schema-row');

CREATE VIEW mcpsec.v_leak AS SELECT id, tag FROM mcpalt.t1;
CREATE VIEW mcpsec.v_secret AS SELECT password_hash FROM mcpsec.secret;

# The DSN carries this run's server port, so hide the statement from the result.
--disable_query_log
--eval SET GLOBAL vsql_mcp.db_url = CONCAT('mysql://mcpuser@127.0.0.1:', @@port)
--enable_query_log
SET GLOBAL vsql_mcp.port = 0;
SET GLOBAL vsql_mcp.vsql_mcp_enabled = ON;

# The worker binds on its next periodic wakeup, so wait for the port to publish
# rather than sleeping.
let $wait_condition=
  SELECT VARIABLE_VALUE > 0 FROM performance_schema.global_status
   WHERE VARIABLE_NAME = 'vsql_mcp.http_port';
--source include/wait_condition.inc
let $mcp_port= `SELECT VARIABLE_VALUE FROM performance_schema.global_status WHERE VARIABLE_NAME = 'vsql_mcp.http_port'`;
