"""Local API for an eval task; started by the harness, not part of the fixture."""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

seen = set()

PAGES = {
    1: {"items": [{"value": 10}, {"value": 20}], "next": 2},
    2: {"items": [{"value": 5}, {"value": 15}, {"value": 25}], "next": 3},
    3: {"items": [{"value": 7}], "next": None},
}


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.end_headers()
        self.wfile.write(body if isinstance(body, bytes) else json.dumps(body).encode())

    def do_GET(self):
        u = urlparse(self.path)
        page = int(parse_qs(u.query).get("page", ["1"])[0])
        if u.path != "/items" or page not in PAGES:
            return self._send(404, b"not found", "text/plain")
        if page not in seen:  # first request per page fails
            seen.add(page)
            return self._send(503, b"service temporarily unavailable", "text/plain")
        self._send(200, PAGES[page])

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", int(os.environ["EVAL_PORT"])), Handler).serve_forever()
