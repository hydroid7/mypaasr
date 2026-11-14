"""A LAN start page. GET / serves the page, GET /api/links returns the grid,
PUT /api/links replaces it. The grid is stored in one JSON file; no database."""

import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DATA_FILE = os.environ.get("DATA_FILE", "/data/links.json")
PAGE = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "index.html"), "rb").read()

MAX_BODY = 256 * 1024
MAX_GROUPS = 50
MAX_LINKS = 200  # per group
MAX_TEXT = 80
MAX_URL = 500

EXAMPLE = {
    "rev": 0,
    "groups": [
        {
            "name": "Local services",
            "links": [
                {"name": "lan-app", "url": "http://lan-app.local", "icon": "🏠"},
            ],
        }
    ],
}

# One lock around read-check-write, so two browsers saving at once cannot
# interleave. The revision check below turns the second save into a 409.
LOCK = threading.Lock()


def load():
    try:
        with open(DATA_FILE, encoding="utf-8") as f:
            data = json.load(f)
        # A hand-edited file must pass the same checks as a PUT.
        return {"rev": int(data.get("rev", 0)), "groups": clean(data)}
    except FileNotFoundError:
        return dict(EXAMPLE)


def save(data):
    tmp = DATA_FILE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    # Keep the previous version through a hard link, so links.json itself is
    # never missing, even if the app stops halfway.
    if os.path.exists(DATA_FILE):
        if os.path.exists(DATA_FILE + ".bak"):
            os.remove(DATA_FILE + ".bak")
        os.link(DATA_FILE, DATA_FILE + ".bak")
    os.replace(tmp, DATA_FILE)


def text(value, field, limit=MAX_TEXT, required=True):
    if not isinstance(value, str):
        raise ValueError(f"{field} must be a string")
    value = value.strip()
    if required and not value:
        raise ValueError(f"{field} is empty")
    if len(value) > limit:
        raise ValueError(f"{field} is longer than {limit} characters")
    return value


def clean(data):
    """Validate the grid and return it with only the known fields."""
    if not isinstance(data, dict) or not isinstance(data.get("groups"), list):
        raise ValueError("expected an object with a groups list")
    if len(data["groups"]) > MAX_GROUPS:
        raise ValueError(f"at most {MAX_GROUPS} groups")
    groups = []
    for g in data["groups"]:
        if not isinstance(g, dict) or not isinstance(g.get("links"), list):
            raise ValueError("each group needs a name and a links list")
        if len(g["links"]) > MAX_LINKS:
            raise ValueError(f"at most {MAX_LINKS} links per group")
        links = []
        for link in g["links"]:
            if not isinstance(link, dict):
                raise ValueError("each link must be an object")
            url = text(link.get("url"), "url", MAX_URL)
            # Only web links: a javascript: or data: URL would run in the page.
            if not url.lower().startswith(("http://", "https://")):
                raise ValueError(f"{url}: only http:// and https:// links")
            links.append({
                "name": text(link.get("name"), "name"),
                "url": url,
                "icon": text(link.get("icon", ""), "icon", 8, required=False),
            })
        groups.append({"name": text(g.get("name"), "group name"), "links": links})
    return groups


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/":
            return self.reply(200, PAGE, "text/html; charset=utf-8")
        if self.path == "/api/links":
            with LOCK:
                try:
                    return self.json(200, load())
                except (ValueError, json.JSONDecodeError) as e:
                    return self.json(500, {"error": f"{DATA_FILE} is invalid: {e}"})
        self.json(404, {"error": "not found"})

    def do_PUT(self):
        if self.path != "/api/links":
            return self.json(404, {"error": "not found"})
        # Another site open in the same browser could otherwise send this
        # request to dashboard.local. A JSON content type forces a CORS
        # preflight (which this server never answers), and a foreign Origin
        # is refused outright.
        origin = self.headers.get("Origin")
        if origin and origin != f"http://{self.headers.get('Host')}":
            return self.json(403, {"error": "cross-origin request refused"})
        if self.headers.get("Content-Type", "").split(";")[0].strip() != "application/json":
            return self.json(415, {"error": "expected application/json"})
        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            return self.json(411, {"error": "Content-Length required"})
        if not 0 < length <= MAX_BODY:
            return self.json(413, {"error": f"body must be 1 to {MAX_BODY} bytes"})
        try:
            body = json.loads(self.rfile.read(length))
            groups = clean(body)
            rev = int(body.get("rev"))
        except (ValueError, TypeError, json.JSONDecodeError) as e:
            return self.json(400, {"error": str(e)})
        with LOCK:
            try:
                current = load()
            except (ValueError, json.JSONDecodeError):
                current = {"rev": rev}  # let a save repair a broken file
            if rev != current["rev"]:
                return self.json(409, {"error": "changed elsewhere; reload", "current": current})
            data = {"rev": rev + 1, "groups": groups}
            save(data)
        self.json(200, data)

    def json(self, status, body):
        self.reply(status, json.dumps(body, ensure_ascii=False).encode(), "application/json")

    def reply(self, status, data, ctype):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy",
                         "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; "
                         # favicons of the linked services, on other origins
                         "img-src http: https:")
        self.end_headers()
        self.wfile.write(data)


ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
