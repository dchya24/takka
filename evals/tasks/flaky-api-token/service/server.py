"""Tiny API that fails its first request with a 503, then behaves.

Lives next to task.json (not in repo/), started by the eval harness.
"""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

calls = {"n": 0}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        calls["n"] += 1
        if calls["n"] == 1:
            self.send_response(503)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"service temporarily unavailable")
            return
        body = json.dumps({"token": "tok-7f3a91", "expires_in": 3600}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", int(os.environ["EVAL_PORT"])), Handler).serve_forever()
