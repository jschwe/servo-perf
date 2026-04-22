#!/usr/bin/env python3
"""HTTPS (HTTP/1.1) fixture server used by servoperf's localhost workloads.

Usage:  https_server.py <port> <doc_root_abs_path>
"""
import http.server
import os
import ssl
import sys
import time


def main():
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <port> <doc_root>", file=sys.stderr)
        sys.exit(2)
    port = int(sys.argv[1])
    doc_root = os.path.abspath(sys.argv[2])
    cert = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cert.pem")
    key = os.path.join(os.path.dirname(os.path.abspath(__file__)), "key.pem")
    os.chdir(doc_root)

    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, fmt, *args):
            print(f"[srv {time.time():.3f}] {fmt % args}", flush=True)

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(certfile=cert, keyfile=key)
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    print(f"listening on https://127.0.0.1:{port}/ doc_root={doc_root}", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
