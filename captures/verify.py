#!/usr/bin/env python3
"""Check that a capture is what was sent, and that the profile is that capture.

The standard this project holds everything to is that every item has a
criterion that can fail. A capture is data, so its criterion is: the bytes on
disk are the bytes the client sent, and the profile is a pure projection of
them. This script is that criterion, and it is meant to be run by a sceptic:

  1. integrity   -- every recorded body hashes to what the proxy's log says it
     recorded, and to what the meta says arrived (a redaction that changed a
     body would show up here as the two hashes differing, not as silence).
  2. consistency -- every request in the run that carried a tools array
     carried the same tools and system.
  3. projection  -- re-deriving {tools, system} from the recorded body
     reproduces the committed profile exactly: same fields, same order, same
     values. Change one character in the profile and this fails.
  4. no secrets  -- no credential from the environment, and nothing shaped
     like one, appears anywhere in the capture.

The scan is deliberately independent of the proxy: it re-reads the files from
disk with no knowledge of what the proxy intended to redact.

Usage:
    verify.py RAW_DIR PROFILE MANIFEST_JSON [--scan PATH ...]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys

# Anything shaped like an API key. Independent of the environment so the check
# still bites when run somewhere the credential is not set.
KEY_SHAPE = re.compile(rb"sk-[A-Za-z0-9_\-]{16,}")


def sha256_file(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def canonical(obj: object) -> str:
    # The same serialization to_profile.py writes with. Duplicated on purpose:
    # a verifier that shares the converter's code cannot catch the converter.
    return json.dumps(obj, indent=2, ensure_ascii=False)


def fail(problems: list[str], label: str, detail: str) -> None:
    problems.append(f"{label}: {detail}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw_dir")
    parser.add_argument("profile")
    parser.add_argument("manifest")
    parser.add_argument("--scan", action="append", default=[])
    args = parser.parse_args()

    problems: list[str] = []
    checks = 0

    with open(args.manifest, encoding="utf-8") as f:
        manifest = json.load(f)
    with open(args.profile, encoding="utf-8") as f:
        profile_text = f.read()
    profile = json.loads(profile_text)

    # 1. Integrity, from the proxy's own log and each request's meta.
    log_path = os.path.join(args.raw_dir, "proxy.log.jsonl")
    log = [json.loads(line) for line in open(log_path, encoding="utf-8") if line.strip()]
    for entry in log:
        checks += 1
        body = os.path.join(args.raw_dir, "requests", f"{entry['seq']:04d}.body.bin")
        if not os.path.exists(body):
            fail(problems, "integrity", f"{body} is in the log but not on disk")
            continue
        actual = sha256_file(body)
        if actual != entry["body_sha256_recorded"]:
            fail(problems, "integrity", f"{body}: sha256 {actual} != log's {entry['body_sha256_recorded']}")
        meta = json.load(open(os.path.join(args.raw_dir, "requests", f"{entry['seq']:04d}.meta.json"), encoding="utf-8"))
        if meta["body_sha256_as_received"] != meta["body_sha256_recorded"]:
            fail(
                problems,
                "integrity",
                f"request {entry['seq']}: redaction changed the body "
                f"({meta['body_sha256_as_received']} != {meta['body_sha256_recorded']})",
            )
        if meta["body_sha256_recorded"] != actual:
            fail(problems, "integrity", f"request {entry['seq']}: meta does not match the body on disk")

    # 2. Consistency across the run: one session, one tool surface.
    tools_seen = []
    system_seen = []
    sessions = set()
    for entry in log:
        if entry["path"].split("?", 1)[0] != "/v1/messages":
            continue
        body_path = os.path.join(args.raw_dir, "requests", f"{entry['seq']:04d}.body.bin")
        body = json.loads(open(body_path, "rb").read())
        user_id = (body.get("metadata") or {}).get("user_id")
        if user_id:
            sessions.add(json.loads(user_id).get("session_id"))
        if body.get("tools"):
            tools_seen.append((entry["seq"], body["tools"]))
            system_seen.append((entry["seq"], body["system"]))
    checks += 1
    if not tools_seen:
        fail(problems, "consistency", "no request in the run carried a tools array")
    else:
        first_seq, first_tools = tools_seen[0]
        for seq, tools in tools_seen[1:]:
            if tools != first_tools:
                fail(problems, "consistency", f"request {seq}'s tools differ from request {first_seq}'s")
        first_system = system_seen[0][1]
        for seq, system in system_seen[1:]:
            if system != first_system:
                fail(problems, "consistency", f"request {seq}'s system differs from request {first_seq}'s")
    if len(sessions) > 1:
        fail(problems, "consistency", f"the capture mixes sessions: {sorted(sessions)}")

    # 2b. Claude Code's own account of the tool list must agree with the wire.
    # `system:init` in the stream-json transcript lists the tools the client
    # believes it advertised; it is written by the client, not by the proxy, so
    # it is an independent check on the tools array. The one known difference
    # is internal naming: the client says "Task" for the tool it sends as
    # "Agent". Anything else is a disagreement worth stopping on.
    transcript = os.path.join(args.raw_dir, "cc-run.stdout.jsonl")
    checks += 1
    if not os.path.exists(transcript):
        fail(problems, "client-agreement", f"{transcript} is missing, so the cross-check cannot run")
    else:
        init = None
        for line in open(transcript, encoding="utf-8"):
            if not line.strip():
                continue
            event = json.loads(line)
            if event.get("type") == "system" and event.get("subtype") == "init":
                init = event
                break
        if init is None or not isinstance(init.get("tools"), list):
            fail(problems, "client-agreement", "no system:init event with a tools list in the transcript")
        elif tools_seen:
            internal_to_wire = {"Task": "Agent"}
            client_tools = [internal_to_wire.get(name, name) for name in init["tools"]]
            wire_tools = [tool["name"] for tool in tools_seen[0][1]]
            if client_tools != wire_tools:
                fail(
                    problems,
                    "client-agreement",
                    f"the client's init list {client_tools} does not match the wire array {wire_tools}",
                )

    # 3. The profile is exactly the projection of the request the manifest names.
    checks += 1
    chosen = os.path.join(args.raw_dir, manifest["request_body"])
    if sha256_file(chosen) != manifest["request_body_sha256"]:
        fail(problems, "projection", f"{chosen} does not hash to the manifest's request_body_sha256")
    else:
        body = json.loads(open(chosen, "rb").read())
        expected = {"tools": body["tools"], "system": body["system"]}
        if canonical(profile) != canonical(expected):
            fail(
                problems,
                "projection",
                "the profile is not the projection of the recorded request "
                "(a field was changed, dropped, reordered or added)",
            )
        if len(profile.get("tools", [])) != manifest["tools"]:
            fail(problems, "projection", "tool count differs from the manifest")

    # 4. No credential -- in the environment or merely key-shaped -- anywhere.
    secrets = [os.environ.get(name, "") for name in ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY")]
    secrets = [s.encode() for s in secrets if s and len(s) >= 8]
    scan_roots = args.scan or [args.raw_dir, args.profile, args.manifest]
    scanned = 0
    for root in scan_roots:
        paths = []
        if os.path.isdir(root):
            for dirpath, _dirs, names in os.walk(root):
                paths += [os.path.join(dirpath, name) for name in names]
        elif os.path.exists(root):
            paths = [root]
        else:
            fail(problems, "secrets", f"--scan target {root} does not exist")
            continue
        for path in paths:
            with open(path, "rb") as f:
                data = f.read()
            scanned += 1
            for secret in secrets:
                if secret in data:
                    fail(problems, "secrets", f"a live credential value appears in {path}")
            hit = KEY_SHAPE.search(data)
            if hit:
                fail(problems, "secrets", f"key-shaped bytes appear in {path}: {hit.group(0)[:6]}...")
    checks += 1

    # 5. The manifest's file hashes still describe the capture on disk.
    checks += 1
    for rel, digest in manifest["capture_files"].items():
        path = os.path.join(args.raw_dir, rel)
        if not os.path.exists(path):
            fail(problems, "manifest", f"{rel} is in the manifest but not on disk")
        elif sha256_file(path) != digest:
            fail(problems, "manifest", f"{rel} changed since the manifest was written")

    if problems:
        print(f"FAIL ({len(problems)} problem(s) over {checks} checks)")
        for problem in problems:
            print("  " + problem)
        return 1
    print(f"OK: {checks} checks passed, {scanned} files scanned for secrets, "
          f"{len(log)} request(s) accounted for")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
