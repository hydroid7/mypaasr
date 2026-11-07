"""Answers with the environment it runs in; /health is for the healthcheck."""

import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ENV = os.environ.get("APP_ENV", "development")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"ok\n" if self.path == "/health" else f"store running in {ENV}\n".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
