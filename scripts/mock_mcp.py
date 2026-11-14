#!/usr/bin/env python3
"""
Minimal Streamable HTTP MCP server for Riggate's local end-to-end tests.

Serves JSON-RPC at /mcp and answers every POST with a plain JSON body (no SSE). Tools:
  echo(text)    returns the text
  add(a, b)     returns a + b
  danger()      advertised so an allowlist has something to refuse

Set MOCK_MCP_TOKEN to require "Authorization: Bearer <token>". Set MOCK_MCP_LOG to a file path
to append one JSON line per tools/call, so a test can assert what the server really received.
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOLS = [
    {
        "name": "echo",
        "description": "Return the given text unchanged.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "Add two numbers.",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        },
    },
    {
        "name": "danger",
        "description": "A tool the operator has not allowed.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def call_tool(name, args):
    if name == "echo":
        return f"echo:{args.get('text', '')}"
    if name == "add":
        return str(args["a"] + args["b"])
    if name == "danger":
        return "danger executed"
    raise KeyError(name)


class McpHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def _send(self, status, payload=None, headers=None):
        data = b"" if payload is None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        if payload is not None:
            self.send_header("Content-Type", "application/json")
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _authorized(self):
        token = os.environ.get("MOCK_MCP_TOKEN")
        return not token or self.headers.get("Authorization") == f"Bearer {token}"

    def do_GET(self):
        # No server-initiated stream; Streamable HTTP clients accept 405 here.
        self._send(405)

    def do_DELETE(self):
        self._send(200)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        if not self._authorized():
            return self._send(401, {"error": "unauthorized"})
        try:
            message = json.loads(raw.decode("utf-8"))
        except Exception:
            return self._send(400, {"error": "bad json"})

        method = message.get("method", "")
        msg_id = message.get("id")
        params = message.get("params") or {}

        if msg_id is None:  # notification
            return self._send(202)

        def reply(result):
            return self._send(
                200, {"jsonrpc": "2.0", "id": msg_id, "result": result},
                {"Mcp-Session-Id": "mock-session"},
            )

        def fail(code, text):
            return self._send(200, {
                "jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": text},
            })

        if method == "initialize":
            return reply({
                "protocolVersion": params.get("protocolVersion", "2025-03-26"),
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "riggate-mock-mcp", "version": "0.1.0"},
            })
        if method == "ping":
            return reply({})
        if method == "tools/list":
            return reply({"tools": TOOLS})
        if method == "tools/call":
            name = params.get("name")
            args = params.get("arguments") or {}
            log_path = os.environ.get("MOCK_MCP_LOG")
            if log_path:
                with open(log_path, "a", encoding="utf-8") as log:
                    log.write(json.dumps({"tool": name, "arguments": args}) + "\n")
            try:
                text = call_tool(name, args)
            except KeyError:
                return fail(-32602, f"unknown tool {name}")
            except Exception as err:  # malformed arguments surface as a tool error
                return reply({"content": [{"type": "text", "text": f"error: {err}"}], "isError": True})
            return reply({"content": [{"type": "text", "text": text}], "isError": False})
        return fail(-32601, f"method not found: {method}")


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    server = ThreadingHTTPServer(("127.0.0.1", port), McpHandler)
    server.daemon_threads = True
    print(f"PORT={server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
