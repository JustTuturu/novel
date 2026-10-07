#!/usr/bin/env python
"""OCR sidecar for the Go `novel` CLI.

PaddleOCR/EasyOCR have no Go runtime, so recognition runs here in a tiny
stdlib HTTP server and the Go binary calls it over localhost.

  GET  /health        -> {"ok": true}
  POST /ocr           {"path": "/path/img.png", "join": "\\n"} -> {"text": "..."}

The Go client (`internal/ocr`) auto-spawns this on port 8765 if it's not
already running; you can also run it directly:

  uv run python sidecar/ocr_service.py --port 8765
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# The Go CLI auto-spawns us from the repo root; make the src/ package importable.
_ROOT = Path(__file__).resolve().parent.parent
_SRC = _ROOT / "src"
if _SRC.is_dir() and str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))

_engine = None


def get_engine():
    """Lazy singleton so the model loads on first request, not at import."""
    global _engine
    if _engine is None:
        from novel.ocr import get_engine as _get

        _engine = _get()
    return _engine


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._send(200, {"ok": True})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/ocr":
            self._send(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            self._send(400, {"error": "bad json"})
            return

        path = req.get("path") or ""
        join = req.get("join", "\n")
        if not path or not os.path.exists(path):
            self._send(400, {"error": f"missing file: {path}"})
            return

        try:
            text = get_engine().image_text(path, join=join)
        except Exception as exc:  # surface the error to the Go client
            self._send(500, {"error": f"{type(exc).__name__}: {exc}"})
            return
        self._send(200, {"text": text})


def main() -> None:
    ap = argparse.ArgumentParser(description="OCR sidecar for novel")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8765)
    args = ap.parse_args()

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[ocr] listening on http://{args.host}:{args.port}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\n[ocr] bye", flush=True)
        srv.server_close()


if __name__ == "__main__":
    main()