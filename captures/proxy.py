#!/usr/bin/env python3
"""Recording reverse proxy for an Anthropic-wire endpoint.

Point a client at this proxy (``ANTHROPIC_BASE_URL=http://127.0.0.1:<port>``) and
it forwards every request to the real endpoint while recording the request
*first*. The capture is therefore the bytes that went out, recorded before the
response was known -- not a reconstruction from a later memory of the run.

Credential handling happens at ingest, the recording point -- the same shape
``crates/yu-wire/src/error.rs`` uses for ``redact_key_material``, which redacts
where the data enters rather than where it is printed:

  * ``authorization``, ``x-api-key`` and friends are replaced by ``****`` before
    anything is written; no code path writes the header value.
  * every recorded string also passes through a literal scrub of the credential
    values named by ``--scrub-env`` -- an exact match, so a credential that
    reaches an unexpected field is caught without guessing at shapes.
  * redaction is visible: each meta file lists what was redacted, and keeps the
    sha256 of the body *as received*, so a reader can see whether redaction
    changed it (in a healthy capture it does not).

Scope note: ``redact_key_material`` also strips the tail a ``****`` mask
preserves and the value after a key-naming phrase. That pass is deliberately
*not* applied to captured text. Its mask-tail rule eats any run of up to
``MAX_MASKED_SUFFIX`` key characters after ``**``, and the first run of this
proxy recorded Claude Code's ExitWorktree description -- "the tool is a
``**no-op**``" -- as "the tool is a ``****``". A capture is the thing being
preserved; a redactor that edits prose trades one failure for another.

Nothing this program prints contains a header value.

Usage:
    proxy.py --upstream https://api.example/anthropic --out DIR [--port 0]

Prints ``LISTEN 127.0.0.1:<port>`` on stdout once it is accepting requests.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import sys
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

# Header names whose value is a credential by definition. Matched lowercased.
SENSITIVE_HEADERS = frozenset(
    (
        "authorization",
        "x-api-key",
        "proxy-authorization",
        "api-key",
        "x-auth-token",
        "cookie",
    )
)

HOP_BY_HOP = frozenset(
    (
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    )
)


# ---------------------------------------------------------------------------
# Redaction, at the recording point. Same discipline as
# `redact_key_material` in crates/yu-wire/src/error.rs -- redact where the
# data enters, not where it is printed -- but narrower in scope; see the
# module docstring for why the mask-tail pass is left out of the capture path.


class Redactor:
    """Replaces named credential values with a visible mask, and only those."""

    def __init__(self, secrets: list[str]):
        # Values that must never reach disk, whatever field they arrive in.
        # Filtered to plausible lengths so an unset or empty var is a no-op.
        self.secrets = [s for s in secrets if s and len(s) >= 8]

    def apply(self, text: str) -> tuple[str, bool]:
        changed = False
        for secret in self.secrets:
            if secret in text:
                text = text.replace(secret, "****")
                changed = True
        return text, changed

    def apply_bytes(self, data: bytes) -> tuple[bytes, bool]:
        if not data:
            return data, False
        # Credentials are ASCII; a body that will not decode as UTF-8 is
        # recorded verbatim rather than corrupted, and the leak scan in
        # verify.py still covers it.
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            return data, False
        redacted, changed = self.apply(text)
        return redacted.encode("utf-8"), changed


# ---------------------------------------------------------------------------
# Recording.


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _write_json(path: str, obj: object) -> None:
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, indent=2, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp, path)


class Record(dict):
    """One recorded request. A dict so it serializes straight to the meta file."""

    def __init__(self, seq: int, directory: str, method: str, path: str):
        super().__init__()
        self.seq = seq
        self.directory = directory
        self.meta_path = os.path.join(directory, "requests", f"{seq:04d}.meta.json")
        self.body_path = os.path.join(directory, "requests", f"{seq:04d}.body.bin")


class Recorder:
    def __init__(self, out_dir: str, redactor: Redactor):
        self.out_dir = out_dir
        self.requests_dir = os.path.join(out_dir, "requests")
        self.log_path = os.path.join(out_dir, "proxy.log.jsonl")
        self.redactor = redactor
        self._lock = threading.Lock()
        os.makedirs(self.requests_dir, exist_ok=True)
        self._seq = self._highest_existing_seq()

    def _highest_existing_seq(self) -> int:
        highest = 0
        for name in os.listdir(self.requests_dir):
            if name.endswith(".meta.json"):
                try:
                    highest = max(highest, int(name.split(".", 1)[0]))
                except ValueError:
                    pass
        return highest

    def begin(
        self, method: str, path: str, request_version: str, headers, body: bytes
    ) -> Record:
        with self._lock:
            self._seq += 1
            seq = self._seq

        raw_items = headers.raw_items() if hasattr(headers, "raw_items") else headers.items()
        recorded_headers: dict[str, str] = {}
        redacted_headers: list[str] = []
        for name, value in raw_items:
            lowered = name.lower()
            if lowered in SENSITIVE_HEADERS:
                recorded_headers[name] = "****"
                if lowered not in redacted_headers:
                    redacted_headers.append(lowered)
                continue
            redacted_value, changed = self.redactor.apply(value)
            recorded_headers[name] = redacted_value
            if changed and lowered not in redacted_headers:
                redacted_headers.append(lowered)

        body_as_received = bytes(body)
        body_recorded, body_redacted = self.redactor.apply_bytes(body)

        record = Record(seq, self.out_dir, method, path)
        record.update(
            {
                "seq": seq,
                "received_at": datetime.now(timezone.utc).isoformat(),
                "method": method,
                "path": path,
                "http_version": request_version,
                "headers": recorded_headers,
                "redacted_headers": redacted_headers,
                "body_bytes": len(body_as_received),
                # Both hashes exist so a reader can see whether redaction
                # touched the body. They differ only when a credential was
                # actually found in it, which is itself the point.
                "body_sha256_as_received": _sha256(body_as_received),
                "body_sha256_recorded": _sha256(body_recorded),
                "body_redacted": bool(body_redacted),
                "outcome": None,
            }
        )

        # Written before the request is forwarded, so the capture cannot depend
        # on the response in any way.
        with open(record.body_path, "wb") as f:
            f.write(body_recorded)
        _write_json(record.meta_path, dict(record))
        return record

    def finish(
        self,
        record: Record,
        status: int | None,
        request_id: str | None,
        latency_ms: int,
        error: str | None,
    ) -> None:
        record["outcome"] = {
            "upstream_status": status,
            "upstream_request_id": request_id,
            "latency_ms": latency_ms,
            "error": error,
        }
        with self._lock:
            _write_json(record.meta_path, dict(record))
            line = {
                "seq": record["seq"],
                "received_at": record["received_at"],
                "method": record["method"],
                "path": record["path"],
                "body_bytes": record["body_bytes"],
                "body_sha256_recorded": record["body_sha256_recorded"],
                "redacted_headers": record["redacted_headers"],
                "body_redacted": record["body_redacted"],
                "upstream_status": status,
                "upstream_request_id": request_id,
                "latency_ms": latency_ms,
                "error": error,
            }
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps(line, ensure_ascii=False) + "\n")


# ---------------------------------------------------------------------------
# The proxy itself.


class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "yu-capture-proxy/1"

    # Silence the default per-request line; the recorder's log is the record.
    def log_message(self, fmt: str, *args) -> None:  # noqa: A003 - stdlib name
        pass

    def do_POST(self) -> None:
        self._proxy("POST")

    def do_GET(self) -> None:
        self._proxy("GET")

    def do_PUT(self) -> None:
        self._proxy("PUT")

    def do_DELETE(self) -> None:
        self._proxy("DELETE")

    def _read_body(self) -> bytes:
        encoding = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in encoding:
            chunks: list[bytes] = []
            while True:
                line = self.rfile.readline().strip()
                size = int(line.split(b";", 1)[0], 16)
                if size == 0:
                    self.rfile.readline()  # trailing CRLF after the last chunk
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)  # CRLF
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def _proxy(self, method: str) -> None:
        self.close_connection = True
        try:
            body = self._read_body()
        except Exception as exc:  # a malformed body is not a capture
            self.send_error(400, f"bad request body: {type(exc).__name__}")
            return

        recorder: Recorder = self.server.recorder  # type: ignore[attr-defined]
        record = recorder.begin(method, self.path, self.request_version, self.headers, body)

        status: int | None = None
        request_id: str | None = None
        error: str | None = None
        started = time.monotonic()
        try:
            status, request_id = self._forward(method, body)
        except Exception as exc:
            error = f"{type(exc).__name__}: {exc}"
            try:
                self.send_response_only(502, "Bad Gateway")
                self.send_header("Content-Type", "text/plain")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(b"capture proxy: upstream request failed\n")
            except Exception:
                pass
        finally:
            recorder.finish(
                record,
                status,
                request_id,
                int((time.monotonic() - started) * 1000),
                error,
            )

    def _forward(self, method: str, body: bytes) -> tuple[int, str | None]:
        upstream = self.server.upstream  # type: ignore[attr-defined]
        path = upstream.path.rstrip("/") + self.path
        conn_cls = (
            http.client.HTTPSConnection
            if upstream.scheme == "https"
            else http.client.HTTPConnection
        )
        conn = conn_cls(
            upstream.hostname,
            upstream.port,
            timeout=self.server.upstream_timeout,  # type: ignore[attr-defined]
        )
        try:
            conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
            for name, value in self.headers.raw_items():
                lowered = name.lower()
                if lowered in HOP_BY_HOP or lowered in ("host", "content-length"):
                    continue
                conn.putheader(name, value)
            conn.putheader("Host", upstream.netloc)
            if body:
                conn.putheader("Content-Length", str(len(body)))
            conn.endheaders(body if body else None)

            resp = conn.getresponse()
            request_id = resp.getheader("request-id") or resp.getheader("x-request-id")
            self.send_response_only(resp.status, resp.reason)
            for name, value in resp.getheaders():
                lowered = name.lower()
                if lowered in HOP_BY_HOP or lowered in ("content-length", "transfer-encoding"):
                    continue
                self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()

            while True:
                chunk = resp.read1(65536)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
            return resp.status, request_id
        finally:
            conn.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", required=True, help="Base URL to forward to")
    parser.add_argument("--out", required=True, help="Capture directory")
    parser.add_argument("--port", type=int, default=0, help="0 picks a free port")
    parser.add_argument(
        "--scrub-env",
        nargs="*",
        default=["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"],
        help="Environment variables whose values must never reach disk",
    )
    parser.add_argument("--upstream-timeout", type=float, default=600.0)
    args = parser.parse_args()

    upstream = urlsplit(args.upstream)
    if upstream.scheme not in ("http", "https") or not upstream.hostname:
        print(f"bad upstream: {args.upstream}", file=sys.stderr)
        return 2

    redactor = Redactor([os.environ.get(name, "") for name in args.scrub_env])
    recorder = Recorder(args.out, redactor)

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Proxy)
    server.daemon_threads = True
    server.upstream = upstream  # type: ignore[attr-defined]
    server.upstream_timeout = args.upstream_timeout  # type: ignore[attr-defined]
    server.recorder = recorder  # type: ignore[attr-defined]

    host, port = server.server_address[:2]
    print(f"LISTEN {host}:{port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
