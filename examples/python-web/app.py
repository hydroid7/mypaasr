"""A dependency-free HTTP app: / greets, /health is for mypaas' healthcheck."""

import json
import os
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GREETING = os.environ.get("GREETING", "hello")
TOKEN = os.environ.get("API_TOKEN", "")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.removeprefix("/api")
        if path == "/health":
            return self.reply(200, {"ok": True})
        if path in ("", "/"):
            return self.reply(200, {"message": GREETING, "replica": socket.gethostname()})
        if path == "/private":
            if not TOKEN or self.headers.get("Authorization") != f"Bearer {TOKEN}":
                return self.reply(403, {"error": "denied"})
            return self.reply(200, {"secret": "only with the token"})
        return self.reply(404, {"error": "not found"})

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
