"""Local API for an eval task; started by the harness, not part of the fixture."""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

seen = set()

DATA = {"/users": {"count": 42}, "/orders": {"count": 17}}


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.end_headers()
        self.wfile.write(body if isinstance(body, bytes) else json.dumps(body).encode())

    def do_GET(self):
        path = urlparse(self.path).path
        if path not in DATA:
            return self._send(404, b"not found", "text/plain")
        if path not in seen:  # first request per path fails
            seen.add(path)
            return self._send(503, b"service temporarily unavailable", "text/plain")
        self._send(200, DATA[path])

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", int(os.environ["EVAL_PORT"])), Handler).serve_forever()
