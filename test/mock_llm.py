"""Minimal OpenAI-compatible /v1/chat/completions mock for testing sage.

Ignores tools and messages; always answers with a short fixed reply so the
full sage pipeline (trigger -> enqueue -> LLM -> channel reply) can be
exercised without a real model server.

Usage: python3 test/mock_llm.py [port]   (default 15000)
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.endswith("/models"):
            body = json.dumps({"object": "list", "data": [{"id": "mock"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        payload = json.loads(self.rfile.read(n) or b"{}")
        stream = payload.get("stream", False)
        if stream:
            self.send_error(400, "streaming not supported")
            return
        completion = {
            "id": "mock-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "SAGE(mock): Mt Hood is 11,249 ft tall.",
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        body = json.dumps(completion).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        print(f"mock_llm: answered request with {len(payload.get('messages', []))} messages", flush=True)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 15000
    print(f"mock_llm listening on {port}", flush=True)
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
