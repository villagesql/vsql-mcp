# Writes the shared MCP client module used by every adversarial test file.
#
# The module is Python 3 standard library only (urllib, json, threading) and is
# written into $MYSQLTEST_VARDIR/tmp so it is per-run and removable. Each test
# writes its own small driver next to it and imports from here.
#
# Every assertion prints exactly one deterministic line, "<name>: PASS" or
# "<name>: FAIL <detail>", so the recorded .result stays stable. Nothing that
# varies per run (ports, timings, session ids, row estimates) is ever printed.
#
# Callers must --remove_file both this module and their driver before the test
# ends, or check-testcase reports the leftover as dirty state.

--write_file $MYSQLTEST_VARDIR/tmp/mcp_client.py
"""Shared MCP Streamable HTTP client for the vsql_mcp MTR suite."""

import json
import sys
import threading
import time
import urllib.error
import urllib.request

PORT = None
BASE = None

PROTOCOL = "2025-06-18"


def configure(port):
    global PORT, BASE
    PORT = str(port)
    BASE = "http://127.0.0.1:%s/mcp" % PORT


def url(path="/mcp"):
    return "http://127.0.0.1:%s%s" % (PORT, path)


def request(body=None, method="POST", headers=None, path="/mcp", raw=None,
            proto=PROTOCOL, origin=None, timeout=120, want_json=True,
            content_type="application/json", accept="application/json, text/event-stream"):
    """One HTTP round trip. Returns (status, parsed_or_text, response_headers).

    Set want_json=False to get the body back as text. A 4xx/5xx is returned the
    same way as a success, never raised, so a test can assert on the status.
    """
    h = {}
    if content_type is not None:
        h["Content-Type"] = content_type
    if accept is not None:
        h["Accept"] = accept
    if proto is not None:
        h["MCP-Protocol-Version"] = proto
    if origin is not None:
        h["Origin"] = origin
    if headers:
        for k, v in headers.items():
            if v is None:
                h.pop(k, None)
            else:
                h[k] = v

    if raw is not None:
        data = raw
    elif body is not None:
        data = json.dumps(body).encode()
    else:
        data = None

    req = urllib.request.Request(url(path), data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            text = r.read().decode(errors="replace")
            status, rhdr = r.status, dict(r.headers)
    except urllib.error.HTTPError as e:
        text = e.read().decode(errors="replace")
        status, rhdr = e.code, dict(e.headers)
    if not want_json:
        return status, text, rhdr
    parsed = None
    if text:
        try:
            parsed = json.loads(text)
        except ValueError:
            parsed = None
    return status, parsed, rhdr


def init(token=None, origin=None):
    """Open a session. Returns the Mcp-Session-Id, or None if not issued."""
    h = {"Authorization": "Bearer " + token} if token else None
    _, _, rhdr = request({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                          "params": {"protocolVersion": PROTOCOL, "capabilities": {}}},
                         headers=h, origin=origin)
    return rhdr.get("Mcp-Session-Id")


def rpc(sid, method, params=None, token=None, timeout=120, rid=2):
    body = {"jsonrpc": "2.0", "id": rid, "method": method}
    if params is not None:
        body["params"] = params
    h = {}
    if sid:
        h["Mcp-Session-Id"] = sid
    if token:
        h["Authorization"] = "Bearer " + token
    return request(body, headers=h, timeout=timeout)


def tool(sid, name, args, token=None, timeout=120):
    """Call a tool. Returns (ok_text_or_None, err_text).

    A tool that succeeded gives (text, ""). A tool that returned isError gives
    (None, message). A transport or JSON-RPC failure gives (None, "<detail>")
    so a caller never has to distinguish None from a missing key.
    """
    st, body, _ = rpc(sid, "tools/call",
                      {"name": name, "arguments": args}, token=token, timeout=timeout)
    if not isinstance(body, dict) or "result" not in body:
        return None, "no result (status %s, body %s)" % (st, json.dumps(body)[:120])
    res = body["result"]
    text = res.get("content", [{}])[0].get("text", "")
    return (None, text) if res.get("isError") else (text, "")


def jtool(sid, name, args, token=None, timeout=120):
    """Call a tool whose success payload is JSON. Returns (obj_or_None, err)."""
    text, err = tool(sid, name, args, token=token, timeout=timeout)
    if text is None:
        return None, err
    try:
        return json.loads(text), ""
    except ValueError:
        return None, "unparseable tool payload"


def query_rows(sid, sql, timeout=120):
    """Rows from the query tool, or None when it errored."""
    obj, _ = jtool(sid, "query", {"sql": sql}, timeout=timeout)
    return None if obj is None else obj.get("rows")


def scalar(sid, sql, column, timeout=120):
    """One cell from a one-row query, or None when it errored or was empty."""
    rows = query_rows(sid, sql, timeout=timeout)
    if not rows:
        return None
    return rows[0].get(column)


def ok(name, cond, detail=""):
    """Print one deterministic assertion line."""
    if cond:
        print("%s: PASS" % name)
    else:
        print("%s: FAIL %s" % (name, str(detail).replace("\n", " ")[:200]))


def rejected(name, err, needle):
    """Assert a tool call was rejected and the message names the guardrail."""
    ok(name, err != "" and needle in err, "err=%r" % (err[:160],))


def burst(fn, n, timeout=180):
    """Run fn(i) on n threads, returning results in submission order."""
    out = [None] * n
    def runner(i):
        try:
            out[i] = fn(i)
        except Exception as e:            # noqa: BLE001 - recorded, not raised
            out[i] = "EXC: %s" % e
    threads = [threading.Thread(target=runner, args=(i,)) for i in range(n)]
    start = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout)
    return out, time.time() - start
EOF
