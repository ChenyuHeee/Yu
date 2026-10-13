#!/usr/bin/env python3
"""capture -> profile: project a recorded request into a tool-surface profile.

What it does, in full:

  1. Walks ``RAW_DIR/requests/*.body.bin`` in sequence order and picks the
     first request whose path is ``/v1/messages`` and whose body carries a
     non-empty ``tools`` array. Later requests in the same session are the
     same conversation with more messages, so the first one is the surface.
  2. Writes ``{"tools": ..., "system": ...}`` -- the two fields DESIGN's
     executor prompt/tool alignment needs -- copied **verbatim**. Keys keep
     their order, nothing is renamed, reordered, defaulted or tidied. The
     only difference from the captured bytes is JSON whitespace.
  3. Writes a manifest naming exactly which request the profile came from and
     the sha256 of every file involved, so the projection can be re-derived
     and checked (see verify.py) rather than trusted.

If the endpoint or Claude Code changes, re-run the capture and re-run this;
the profile is a snapshot, not a source of truth.

Usage:
    to_profile.py RAW_DIR OUT_JSON MANIFEST_JSON
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import sys


def load_body(meta_path: str) -> tuple[object, bytes]:
    with open(meta_path, encoding="utf-8") as f:
        meta = json.load(f)
    body_path = os.path.join(os.path.dirname(meta_path), os.path.basename(meta_path).replace(".meta.json", ".body.bin"))
    with open(body_path, "rb") as f:
        raw = f.read()
    headers = {name.lower(): value for name, value in meta.get("headers", {}).items()}
    if headers.get("content-encoding", "").lower() == "gzip":
        raw = gzip.decompress(raw)
    return meta, raw


def sha256_file(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw_dir", help="A capture directory written by proxy.py")
    parser.add_argument("out_json", help="Where to write the profile")
    parser.add_argument("manifest_json", help="Where to write the provenance manifest")
    args = parser.parse_args()

    requests_dir = os.path.join(args.raw_dir, "requests")
    metas = sorted(
        (name for name in os.listdir(requests_dir) if name.endswith(".meta.json")),
        key=lambda name: int(name.split(".", 1)[0]),
    )

    chosen = None
    for name in metas:
        meta, raw = load_body(os.path.join(requests_dir, name))
        path = meta["path"].split("?", 1)[0]
        if path != "/v1/messages":
            continue
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if body.get("tools"):
            chosen = (meta, raw, body)
            break

    if chosen is None:
        print("no /v1/messages request with a non-empty tools array", file=sys.stderr)
        return 1

    meta, raw, body = chosen
    if "system" not in body:
        print("the chosen request has no system field", file=sys.stderr)
        return 1

    profile = {"tools": body["tools"], "system": body["system"]}
    with open(args.out_json, "w", encoding="utf-8") as f:
        json.dump(profile, f, indent=2, ensure_ascii=False)
        f.write("\n")

    # Every file of the capture, so a reader can check that nothing moved under
    # the profile after it was taken.
    files = {}
    for dirpath, _dirs, names in os.walk(args.raw_dir):
        for name in sorted(names):
            full = os.path.join(dirpath, name)
            files[os.path.relpath(full, args.raw_dir)] = sha256_file(full)

    manifest = {
        "profile": os.path.basename(args.out_json),
        "profile_sha256": sha256_file(args.out_json),
        "request": meta["seq"],
        "request_path": meta["path"],
        "request_body": f"requests/{meta['seq']:04d}.body.bin",
        "request_body_sha256": sha256_file(
            os.path.join(requests_dir, f"{meta['seq']:04d}.body.bin")
        ),
        "request_body_bytes": len(raw),
        "tools": len(profile["tools"]),
        "tool_names": [tool["name"] for tool in profile["tools"]],
        "capture_files": files,
    }
    with open(args.manifest_json, "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2, ensure_ascii=False)
        f.write("\n")

    print(f"profile: {args.out_json}  ({len(profile['tools'])} tools)")
    print(f"from:    request {meta['seq']} ({meta['path']})")
    print("tools:   " + ", ".join(manifest["tool_names"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
