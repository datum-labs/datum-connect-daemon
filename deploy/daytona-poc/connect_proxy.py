#!/usr/bin/env python3
"""Minimal HTTP CONNECT proxy, for the Daytona egress-workaround experiment.

Listens locally; a Datum Cloud tunnel exposes this publicly. Daytona's
outbound_proxy_url points sandboxes at the tunnel's public hostname, and
Daytona's internal egress proxy speaks HTTP CONNECT to it -- this is the
only side of that handshake we need to implement ourselves.
"""
import socket
import sys
import threading

LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = 8080


def relay(a: socket.socket, b: socket.socket) -> None:
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    finally:
        try:
            b.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(client: socket.socket, addr) -> None:
    try:
        request = b""
        while b"\r\n\r\n" not in request:
            chunk = client.recv(4096)
            if not chunk:
                client.close()
                return
            request += chunk

        line = request.split(b"\r\n", 1)[0].decode("latin1")
        print(f"[{addr}] {line}")
        parts = line.split()
        if len(parts) < 2 or parts[0] != "CONNECT":
            client.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            client.close()
            return

        host, _, port = parts[1].partition(":")
        port = int(port) if port else 443

        try:
            upstream = socket.create_connection((host, port), timeout=10)
        except OSError as e:
            print(f"[{addr}] upstream connect to {host}:{port} failed: {e}")
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            client.close()
            return

        client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")

        t1 = threading.Thread(target=relay, args=(client, upstream), daemon=True)
        t2 = threading.Thread(target=relay, args=(upstream, client), daemon=True)
        t1.start()
        t2.start()
        t1.join()
        t2.join()
        upstream.close()
    except Exception as e:
        print(f"[{addr}] error: {e}", file=sys.stderr)
    finally:
        client.close()


def main() -> None:
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((LISTEN_HOST, LISTEN_PORT))
    srv.listen(50)
    print(f"CONNECT proxy listening on {LISTEN_HOST}:{LISTEN_PORT}")
    while True:
        client, addr = srv.accept()
        threading.Thread(target=handle, args=(client, addr), daemon=True).start()


if __name__ == "__main__":
    main()
