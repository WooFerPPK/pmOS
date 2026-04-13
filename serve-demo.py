#!/usr/bin/env python3
"""
PMos demo static server.

A ~100-line dependency-free HTTP server that serves `dist/` on
http://localhost:8080 with the COOP/COEP headers required for
SharedArrayBuffer and cross-origin isolation.

Usage:
    python3 serve-demo.py
    python3 serve-demo.py --port 9000
    python3 serve-demo.py --dir some/other/path
    python3 serve-demo.py --host 0.0.0.0        # listen on all interfaces

This is the replacement for `just dev` when the Rust toolchain
is not yet installed. Once you have Rust + cargo + just, run
`just dev` instead — it uses the xtask dev-server in
crates/xtask/src/dev_server.rs which does the same thing but as
part of the real build pipeline.
"""

from __future__ import annotations

import argparse
import http.server
import mimetypes
import os
import socketserver
import sys
from pathlib import Path


# Ensure .wasm is served with the correct MIME type. Python's
# default mimetypes.types_map doesn't always include it.
mimetypes.add_type("application/wasm", ".wasm")
mimetypes.add_type("application/javascript", ".js")
mimetypes.add_type("application/javascript", ".mjs")
mimetypes.add_type("text/html; charset=utf-8", ".html")
mimetypes.add_type("application/json", ".json")


class PMosHandler(http.server.SimpleHTTPRequestHandler):
    """
    SimpleHTTPRequestHandler subclass that:
      * Sets COOP / COEP / CORP headers on every response.
      * Disables caching so edit-and-refresh works during dev.
      * Serves `dist/` from the dir passed in via constructor.
    """

    def __init__(self, *args, directory: str | None = None, **kwargs):
        super().__init__(*args, directory=directory, **kwargs)

    def end_headers(self) -> None:
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, format: str, *args) -> None:  # noqa: A002
        sys.stderr.write("[pmos-serve] %s - %s\n" % (self.address_string(), format % args))


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="PMos demo static server")
    p.add_argument(
        "--dir",
        default=str(Path(__file__).resolve().parent / "dist"),
        help="Directory to serve (default: ./dist next to this script)",
    )
    p.add_argument(
        "--port",
        type=int,
        default=8080,
        help="TCP port to listen on (default: 8080)",
    )
    p.add_argument(
        "--host",
        default="127.0.0.1",
        help='Host to bind on (default: 127.0.0.1; use "0.0.0.0" for LAN access)',
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()
    root = Path(args.dir).resolve()
    if not root.is_dir():
        print(f"[pmos-serve] ERROR: --dir {root} does not exist", file=sys.stderr)
        return 1
    index = root / "index.html"
    if not index.is_file():
        print(
            f"[pmos-serve] ERROR: {index} not found; did you build dist/?",
            file=sys.stderr,
        )
        return 1

    os.chdir(root)
    print(f"[pmos-serve] serving {root}")
    print(f"[pmos-serve] listening on http://{args.host}:{args.port}")
    print(f"[pmos-serve] COOP: same-origin  COEP: require-corp  CORP: same-origin")
    print(f"[pmos-serve] Ctrl-C to stop")

    handler = lambda *a, **kw: PMosHandler(*a, directory=str(root), **kw)  # noqa: E731

    socketserver.TCPServer.allow_reuse_address = True
    try:
        with socketserver.TCPServer((args.host, args.port), handler) as httpd:
            httpd.serve_forever()
    except KeyboardInterrupt:
        print("\n[pmos-serve] bye")
        return 0
    except OSError as e:
        print(f"[pmos-serve] ERROR: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
