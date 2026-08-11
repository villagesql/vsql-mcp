# VillageSQL MCP Extension

Exposes a VillageSQL database as a [Model Context Protocol](https://modelcontextprotocol.io)
server, so MCP clients (Claude Code, Claude Desktop, IDE agents) can discover
the schema and run governed queries without a sidecar process.

```sql
INSTALL EXTENSION vsql_mcp;
```

Full documentation is assembled at the end of the build workflow.
