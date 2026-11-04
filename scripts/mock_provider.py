#!/usr/bin/env python3
"""
Lightweight mock OpenAI-compatible chat completion provider for local testing.
Listens on 127.0.0.1 and returns valid OpenAI chat completion responses.
"""

import sys
import json
from http.server import HTTPServer, BaseHTTPRequestHandler

class MockOpenAIHandler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        # Suppress default request logging to keep output clean
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length > 0 else b"{}"
        try:
            req_data = json.loads(body.decode("utf-8"))
        except Exception:
            req_data = {}

        model = req_data.get("model", "mock-model")
        response = {
            "id": "chatcmpl-mock-12345",
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": f"Mock response from upstream for model '{model}'"
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20
            }
        }

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(response).encode("utf-8"))

def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    server = HTTPServer(("127.0.0.1", port), MockOpenAIHandler)
    assigned_port = server.server_address[1]
    # Print assigned port so the caller script can read it
    print(f"PORT={assigned_port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()

if __name__ == "__main__":
    main()
