#!/usr/bin/env python3
"""Home-side config/data broker for the Daytona immutable-image POC.

Bound to 127.0.0.1 only, deliberately -- this process never gets a public
IP or an open port of its own. It's reachable exclusively through the
Datum peer tunnel advertised on top of it (see NOTES.md's "Immutable
sandbox image POC" section for the full design). That's the point being
demonstrated: an ephemeral, third-party-hosted sandbox can reach a
service that was never exposed to the public internet at all.

GET /config   -> a small JSON blob, unique per broker restart, so a real
                 fetch is distinguishable from a stale/cached one.
POST /data    -> durably appended (one JSON line per call) to data_log,
                 alongside a server-side received-at timestamp, so a
                 real round trip is distinguishable from a claimed one.
"""

import http.server
import json
import socketserver
import sys
import time
import uuid

PORT = 18081
DATA_LOG = "sandbox_data_log.jsonl"

# Regenerated each time the broker starts -- proves a fetch happened
# against *this* run of the broker, not a memory of an earlier one.
CONFIG = {
    "broker_instance_id": str(uuid.uuid4()),
    "broker_started_at_unix": int(time.time()),
    "message": "hello from the home-side broker",
    "demo_setting": "immutable-image-poc",
}


class Handler(http.server.BaseHTTPRequestHandler):
    def _send_json(self, status, body):
        payload = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path == "/config":
            self._send_json(200, CONFIG)
        else:
            self._send_json(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/data":
            self._send_json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            self._send_json(400, {"error": "invalid json"})
            return

        record = {"received_at_unix": int(time.time()), "payload": body}
        with open(DATA_LOG, "a") as f:
            f.write(json.dumps(record) + "\n")

        self._send_json(200, {"stored": True})

    def log_message(self, fmt, *args):
        sys.stderr.write("[broker] " + (fmt % args) + "\n")


if __name__ == "__main__":
    with socketserver.TCPServer(("127.0.0.1", PORT), Handler) as httpd:
        print(f"[broker] listening on 127.0.0.1:{PORT}, config instance {CONFIG['broker_instance_id']}", flush=True)
        httpd.serve_forever()
