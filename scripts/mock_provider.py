#!/usr/bin/env python3
"""
Scripted mock OpenAI-compatible provider for local testing.

Listens on 127.0.0.1 and speaks three dialects, streaming or not: the OpenAI Responses API
(POST .../responses, which is what Riggate's `openai` provider sends today), OpenAI Chat
Completions (POST .../chat/completions), and the Anthropic Messages API (POST .../messages).
By default it answers every request with a fixed sentence. Directives placed in the LAST
user message script richer behaviour, one directive per line:

  MOCK_TOOL <name> <json-args>   call the tool (several lines = parallel calls); once the
                                 tool results come back, answer "Tool result: <results>"
  MOCK_LOOP <name> <json-args>   call the tool on every turn and never answer
                                 (exercises an agent's max_turns limit)
  MOCK_STATUS <code>             fail with this HTTP status (upstream failure)
  MOCK_SLEEP <seconds>           delay the response (upstream timeout)
  MOCK_CHATBODY                  reply with a Chat Completions body whatever endpoint was called
                                 (an upstream that ignores the Responses API)
  MOCK_GARBAGE                   reply 200 with a body in no dialect (a misconfigured upstream)

Set MOCK_REQUEST_LOG to a file path to append every request (path, auth headers, body) as one
JSON line, so a test can assert what Riggate sent upstream (history, offered tools, credentials).
"""

import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DIRECTIVE = re.compile(r"^MOCK_(TOOL|LOOP|STATUS|SLEEP|GARBAGE|CHATBODY)\s*(.*)$", re.MULTILINE)


def text_of(content):
    """Message content may be a string or a list of typed parts."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def last_user_text(messages):
    for message in reversed(messages):
        if message.get("role") == "user":
            return text_of(message.get("content"))
    return ""


def tool_results_after_last_user(messages):
    results = []
    for message in reversed(messages):
        if message.get("role") == "user":
            break
        if message.get("role") == "tool":
            results.append(text_of(message.get("content")))
    return list(reversed(results))


def anthropic_messages(req):
    """Anthropic carries tool results as blocks inside user messages; split them out."""
    messages = []
    for message in req.get("messages", []):
        content = message.get("content")
        if message.get("role") != "user" or isinstance(content, str):
            messages.append({"role": message.get("role"), "content": text_of(content)})
            continue
        texts = [b.get("text", "") for b in content if b.get("type") == "text"]
        if texts:
            messages.append({"role": "user", "content": "".join(texts)})
        for block in content:
            if block.get("type") == "tool_result":
                messages.append({"role": "tool", "content": text_of(block.get("content"))})
    return messages


def to_messages(req, anthropic=False):
    """Reduce any dialect to chat-style messages: user text and tool results."""
    if anthropic:
        return anthropic_messages(req)
    if "messages" in req:
        return req["messages"]
    items = req.get("input", [])
    if isinstance(items, str):
        return [{"role": "user", "content": items}]
    messages = []
    for item in items:
        kind = item.get("type", "message")
        if kind == "function_call_output":
            messages.append({"role": "tool", "content": item.get("output", "")})
        elif kind == "message":
            messages.append({"role": item.get("role"), "content": text_of(item.get("content"))})
    return messages


def parse_directives(messages):
    return [(kind, rest.strip()) for kind, rest in DIRECTIVE.findall(last_user_text(messages))]


def split_call(rest):
    name, _, args = rest.partition(" ")
    return name, (args.strip() or "{}")


class MockOpenAIHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def _json(self, status, payload):
        data = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._json(200, {"status": "ok"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length > 0 else b"{}"
        try:
            req = json.loads(body.decode("utf-8"))
        except Exception:
            req = {}

        log_path = os.environ.get("MOCK_REQUEST_LOG")
        if log_path:
            with open(log_path, "a", encoding="utf-8") as log:
                log.write(json.dumps({
                    "path": self.path,
                    "x_api_key": self.headers.get("x-api-key"),
                    "authorization": self.headers.get("Authorization"),
                    "anthropic_version": self.headers.get("anthropic-version"),
                    "body": req,
                }) + "\n")

        model = req.get("model", "mock-model")
        path = self.path.rstrip("/")
        anthropic_api = path.endswith("/messages")
        messages = to_messages(req, anthropic_api)
        responses_api = path.endswith("/responses")
        directives = parse_directives(messages)
        results = tool_results_after_last_user(messages)

        for kind, rest in directives:
            if kind == "SLEEP":
                time.sleep(float(rest or 0))
            elif kind == "CHATBODY":
                return self._respond_text(req, model, "wrong dialect", False, False, force_chat=True)
            elif kind == "GARBAGE":
                return self._json(200, {"hello": "this is not a model response"})
            elif kind == "STATUS":
                return self._json(
                    int(rest or 500),
                    {"error": {"message": "scripted upstream failure", "type": "mock_error"}},
                )

        calls = []
        for kind, rest in directives:
            if kind == "LOOP" or (kind == "TOOL" and not results):
                name, args = split_call(rest)
                calls.append({"name": name, "arguments": args})

        if calls:
            return self._respond_tool_calls(req, model, calls, responses_api, anthropic_api)

        if results:
            text = "Tool result: " + " | ".join(results)
        else:
            text = f"Mock response from upstream for model '{model}'"
        return self._respond_text(req, model, text, responses_api, anthropic_api)

    def _usage(self):
        return {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}

    def _respond_text(self, req, model, text, responses_api=False, anthropic_api=False,
                      force_chat=False):
        if anthropic_api and not force_chat:
            return self._anthropic(req, model, [{"type": "text", "text": text}], "end_turn")
        if responses_api and not force_chat:
            return self._responses(req, model, [self._message_item(text)])
        if req.get("stream"):
            chunks = [
                self._chunk(model, {"role": "assistant", "content": ""}, None),
                self._chunk(model, {"content": text}, None),
                self._chunk(model, {}, "stop"),
            ]
            return self._sse(req, model, chunks)
        self._json(200, {
            "id": "chatcmpl-mock-12345",
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop",
            }],
            "usage": self._usage(),
        })

    def _respond_tool_calls(self, req, model, calls, responses_api=False, anthropic_api=False):
        if anthropic_api:
            blocks = [
                {"type": "tool_use", "id": f"toolu_mock_{i}", "name": c["name"],
                 "input": json.loads(c["arguments"])}
                for i, c in enumerate(calls)
            ]
            return self._anthropic(req, model, blocks, "tool_use")
        if responses_api:
            items = [
                {"type": "function_call", "id": f"fc_mock_{i}", "call_id": f"call_mock_{i}",
                 "name": c["name"], "arguments": c["arguments"], "status": "completed"}
                for i, c in enumerate(calls)
            ]
            return self._responses(req, model, items)
        wire = [
            {
                "id": f"call_mock_{i}",
                "type": "function",
                "function": {"name": c["name"], "arguments": c["arguments"]},
            }
            for i, c in enumerate(calls)
        ]
        if req.get("stream"):
            chunks = [self._chunk(model, {"role": "assistant", "content": None}, None)]
            for i, call in enumerate(wire):
                chunks.append(self._chunk(model, {"tool_calls": [{
                    "index": i,
                    "id": call["id"],
                    "type": "function",
                    "function": {"name": call["function"]["name"], "arguments": ""},
                }]}, None))
                chunks.append(self._chunk(model, {"tool_calls": [{
                    "index": i,
                    "function": {"arguments": call["function"]["arguments"]},
                }]}, None))
            chunks.append(self._chunk(model, {}, "tool_calls"))
            return self._sse(req, model, chunks)
        self._json(200, {
            "id": "chatcmpl-mock-12345",
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": None, "tool_calls": wire},
                "finish_reason": "tool_calls",
            }],
            "usage": self._usage(),
        })

    def _anthropic(self, req, model, blocks, stop_reason):
        usage = {"input_tokens": 12, "output_tokens": 8}
        message = {
            "id": "msg_mock_1", "type": "message", "role": "assistant", "model": model,
            "content": blocks, "stop_reason": stop_reason, "stop_sequence": None, "usage": usage,
        }
        if not req.get("stream"):
            return self._json(200, message)
        events = [("message_start", {"message": dict(message, content=[], stop_reason=None)})]
        for index, block in enumerate(blocks):
            if block["type"] == "text":
                events.append(("content_block_start", {
                    "index": index, "content_block": {"type": "text", "text": ""}}))
                events.append(("content_block_delta", {
                    "index": index, "delta": {"type": "text_delta", "text": block["text"]}}))
            else:
                events.append(("content_block_start", {"index": index, "content_block": {
                    "type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}}))
                events.append(("content_block_delta", {"index": index, "delta": {
                    "type": "input_json_delta", "partial_json": json.dumps(block["input"])}}))
            events.append(("content_block_stop", {"index": index}))
        events.append(("message_delta", {
            "delta": {"stop_reason": stop_reason, "stop_sequence": None},
            "usage": {"output_tokens": 8}}))
        events.append(("message_stop", {}))
        payload = "".join(
            f"event: {name}\ndata: {json.dumps(dict(data, type=name))}\n\n" for name, data in events
        )
        data = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _message_item(self, text):
        return {
            "type": "message", "id": "msg_mock_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }

    def _response_object(self, model, output):
        return {
            "id": "resp_mock_1", "object": "response", "created_at": 1700000000,
            "status": "completed", "model": model, "output": output,
            "usage": {
                "input_tokens": 12, "output_tokens": 8, "total_tokens": 20,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            },
        }

    def _responses(self, req, model, output):
        response = self._response_object(model, output)
        if not req.get("stream"):
            return self._json(200, response)
        events = [("response.created", {"response": dict(response, status="in_progress", output=[])})]
        for index, item in enumerate(output):
            started = dict(item, status="in_progress")
            if item["type"] == "message":
                started["content"] = []
                events.append(("response.output_item.added",
                               {"output_index": index, "item": started}))
                events.append(("response.output_text.delta", {
                    "item_id": item["id"], "output_index": index, "content_index": 0,
                    "delta": item["content"][0]["text"],
                }))
            else:
                started["arguments"] = ""
                events.append(("response.output_item.added",
                               {"output_index": index, "item": started}))
                events.append(("response.function_call_arguments.delta", {
                    "item_id": item["id"], "output_index": index, "delta": item["arguments"],
                }))
            events.append(("response.output_item.done", {"output_index": index, "item": item}))
        events.append(("response.completed", {"response": response}))
        payload = "".join(
            f"event: {name}\ndata: {json.dumps(dict(data, type=name))}\n\n" for name, data in events
        )
        data = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _chunk(self, model, delta, finish_reason):
        return {
            "id": "chatcmpl-mock-12345",
            "object": "chat.completion.chunk",
            "created": 1700000000,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        }

    def _sse(self, req, model, chunks):
        if (req.get("stream_options") or {}).get("include_usage"):
            chunks.append({
                "id": "chatcmpl-mock-12345",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": model,
                "choices": [],
                "usage": self._usage(),
            })
        payload = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        data = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    server = ThreadingHTTPServer(("127.0.0.1", port), MockOpenAIHandler)
    server.daemon_threads = True
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
