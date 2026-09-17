#!/usr/bin/env python3
"""Local relay for viewing a datumproxy.net tunnel in a real desktop browser.

Real browsers negotiate HTTP/2 via ALPN, and the Datum proxy edge corrupts
responses over HTTP/2 (transparent compression + stripped Content-Encoding/
Content-Type, worse than the HTTP/1.1 case). This relay does what the Android
app's OkHttp interceptor does: fetch over HTTP/1.1 with identity encoding and
re-serve with an explicit, correct Content-Type, so any normal browser hitting
http://127.0.0.1:PORT/ sees the tunnel content correctly.

Usage: python pc_relay.py <tunnel-hostname> [port]
"""
import sys
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

if len(sys.argv) < 2:
    print("usage: python pc_relay.py <tunnel-hostname> [port]")
    sys.exit(1)

TUNNEL_HOST = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8090


class RelayHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        upstream_url = f"https://{TUNNEL_HOST}{self.path}"
        req = urllib.request.Request(
            upstream_url,
            headers={"Accept-Encoding": "identity"},
        )
        try:
            with urllib.request.urlopen(req, timeout=20) as resp:
                body = resp.read()
        except Exception as e:
            self.send_response(502)
            self.end_headers()
            self.wfile.write(f"relay fetch failed: {e}".encode())
            return

        mime = "image/jpeg" if self.path.endswith("/photo.jpg") else "text/html"
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print(f"[relay] {self.address_string()} - {fmt % args}")


if __name__ == "__main__":
    print(f"Relaying http://127.0.0.1:{PORT}/ -> https://{TUNNEL_HOST}/ (HTTP/1.1, identity encoding)")
    HTTPServer(("127.0.0.1", PORT), RelayHandler).serve_forever()
