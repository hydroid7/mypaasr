"""A guestbook on SQLite: GET / lists entries, POST / with a body adds one."""

import json
import os
import sqlite3
from http.server import BaseHTTPRequestHandler, HTTPServer

DB = sqlite3.connect(os.environ.get("DB_PATH", "/data/guestbook.db"), check_same_thread=False)
DB.execute("PRAGMA journal_mode=WAL")
DB.execute("CREATE TABLE IF NOT EXISTS entries (id INTEGER PRIMARY KEY, text TEXT NOT NULL)")
DB.commit()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        rows = DB.execute("SELECT text FROM entries ORDER BY id DESC LIMIT 50").fetchall()
        self.reply(200, {"entries": [r[0] for r in rows]})

    def do_POST(self):
        length = min(int(self.headers.get("Content-Length", 0)), 1000)
        text = self.rfile.read(length).decode("utf-8", "replace").strip()
        if not text:
            return self.reply(400, {"error": "empty entry"})
        DB.execute("INSERT INTO entries (text) VALUES (?)", (text,))
        DB.commit()
        self.reply(201, {"ok": True})

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


# A single-threaded server is enough here and keeps SQLite access simple.
HTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
