#!/usr/bin/env bash
# Probe 2: the constraints that shape the loop itself.
#
# Probe 1 established: thinking is always emitted and MUST be echoed back (400 otherwise).
# This round pins the exact replay shape, plus auth enforcement and parallel tool use.

set -uo pipefail
BASE="${ANTHROPIC_BASE_URL:?}"
TOK="${ANTHROPIC_AUTH_TOKEN:?}"
MODEL="${ANTHROPIC_MODEL:-deepseek-flash[1m]}"
OUT="${1:-/tmp/yu-probe2}"
mkdir -p "$OUT"

send() { # send <name> <body> [auth-override]
  local name="$1" body="$2" auth="${3:-$TOK}"
  curl -sS --max-time 90 -o "$OUT/$name.body" -w '%{http_code}' \
    -X POST "$BASE/v1/messages" \
    -H 'content-type: application/json' \
    -H "x-api-key: $auth" \
    -H 'anthropic-version: 2023-06-01' \
    -d "$body" > "$OUT/$name.code" 2>"$OUT/$name.err" || echo 000 > "$OUT/$name.code"
}

TOOLS='[{"name":"get_time","description":"Return the current time in a named timezone.","input_schema":{"type":"object","properties":{"tz":{"type":"string"}},"required":["tz"]}},{"name":"get_weather","description":"Return the current weather for a city.","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}]'

# 1. Capture a genuine tool-use turn (non-streaming) so we can replay real bytes.
send capture_tooluse "{\"model\":\"$MODEL\",\"max_tokens\":2000,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"}]}"

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
d = json.load(open(os.path.join(out, "capture_tooluse.body")))
asst = [b for b in d["content"] if b["type"] in ("thinking", "text", "tool_use")]
open(os.path.join(out, "asst_full.json"), "w").write(json.dumps(asst))
open(os.path.join(out, "asst_nothink.json"), "w").write(
    json.dumps([b for b in asst if b["type"] != "thinking"]))
# thinking blocks with the signature field removed entirely
stripped = []
for b in asst:
    if b["type"] == "thinking":
        b = {k: v for k, v in b.items() if k != "signature"}
    stripped.append(b)
open(os.path.join(out, "asst_nosig.json"), "w").write(json.dumps(stripped))
# thinking block with an empty signature
emptied = []
for b in asst:
    if b["type"] == "thinking":
        b = dict(b, signature="")
    emptied.append(b)
open(os.path.join(out, "asst_emptysig.json"), "w").write(json.dumps(emptied))
tu = [b for b in asst if b["type"] == "tool_use"]
if not tu:
    print("NO_TOOL_USE_CAPTURED")
else:
    open(os.path.join(out, "tu_id.txt"), "w").write(tu[0]["id"])
    print("tool_use id:", tu[0]["id"])
    print("blocks:", [b["type"] for b in asst])
    print("thinking len:", len(next((b["thinking"] for b in asst if b["type"]=="thinking"), "")),
          "sig:", repr(next((b.get("signature") for b in asst if b["type"]=="thinking"), None))[:60])
PY

TUID=$(cat "$OUT/tu_id.txt" 2>/dev/null || echo "toolu_x")
ASST_FULL=$(cat "$OUT/asst_full.json")
ASST_NOTHINK=$(cat "$OUT/asst_nothink.json")
ASST_NOSIG=$(cat "$OUT/asst_nosig.json")
ASST_EMPTYSIG=$(cat "$OUT/asst_emptysig.json")

replay() { # replay <name> <assistant-content-json>
  send "$1" "{\"model\":\"$MODEL\",\"max_tokens\":100,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"},{\"role\":\"assistant\",\"content\":$2},{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$TUID\",\"content\":\"14:32 JST\"}]}]}"
}

replay replay_full     "$ASST_FULL"       # expect 200
replay replay_nothink  "$ASST_NOTHINK"    # expect 400
replay replay_nosig    "$ASST_NOSIG"      # signature field absent
replay replay_emptysig "$ASST_EMPTYSIG"   # signature present but empty

# 2. Is auth actually enforced?
send auth_garbage "{\"model\":\"$MODEL\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}" "sk-definitely-not-a-real-key-000000000000"

# 3. Parallel tool use: two independent questions, does one turn carry two tool_use blocks?
send parallel_tools "{\"model\":\"$MODEL\",\"max_tokens\":2000,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"I need two things at once: the time in Tokyo AND the weather in Paris. Call both tools in the same turn.\"}]}"

# 4. Does a tool_result accept is_error, and does the model recover?
send tool_error "{\"model\":\"$MODEL\",\"max_tokens\":300,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo?\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"call_probe_err\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]},{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"call_probe_err\",\"is_error\":true,\"content\":\"Error: timezone database unavailable\"}]}]}"

# 5. Server-side web search - the only Anthropic-defined tool this endpoint accepts.
send websearch "{\"model\":\"$MODEL\",\"max_tokens\":600,\"tools\":[{\"type\":\"web_search_20260209\",\"name\":\"web_search\",\"max_uses\":2}],\"messages\":[{\"role\":\"user\",\"content\":\"Search the web for the current Rust stable version and report it.\"}]}"

# 6. Long system prompt: does cache_control produce a cache write, or is caching automatic?
LONG=$(python3 -c "print('You are a meticulous assistant. ' * 400)")
send cache_probe_1 "{\"model\":\"$MODEL\",\"max_tokens\":16,\"system\":[{\"type\":\"text\",\"text\":\"$LONG\",\"cache_control\":{\"type\":\"ephemeral\"}}],\"messages\":[{\"role\":\"user\",\"content\":\"Say ok\"}]}"
send cache_probe_2 "{\"model\":\"$MODEL\",\"max_tokens\":16,\"system\":[{\"type\":\"text\",\"text\":\"$LONG\",\"cache_control\":{\"type\":\"ephemeral\"}}],\"messages\":[{\"role\":\"user\",\"content\":\"Say ok\"}]}"

# 7. max_tokens truncation mid-thinking - what does the partial block look like?
send truncation "{\"model\":\"$MODEL\",\"max_tokens\":8,\"messages\":[{\"role\":\"user\",\"content\":\"Explain the theory of relativity in detail.\"}]}"

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
names = ["capture_tooluse","replay_full","replay_nothink","replay_nosig","replay_emptysig",
         "auth_garbage","parallel_tools","tool_error","websearch",
         "cache_probe_1","cache_probe_2","truncation"]
w = 18
print()
print("=" * 108)
print("  PROBE 2  -  loop-shaping constraints")
print("=" * 108)
for n in names:
    try:
        code = open(os.path.join(out, n + ".code")).read().strip()
    except OSError:
        code = "???"
    try:
        d = json.loads(open(os.path.join(out, n + ".body"), "rb").read())
    except Exception as e:
        print(f"  {'----':<4} {n:<{w}} {code:>4}  unparseable: {e}")
        continue
    if isinstance(d, dict) and "error" in d:
        msg = str(d["error"].get("message", ""))[:88]
        print(f"  FAIL {n:<{w}} {code:>4}  {d['error'].get('type','?')}: {msg}")
        continue
    u = d.get("usage") or {}
    blocks = [b.get("type") for b in (d.get("content") or [])]
    ntu = sum(1 for b in (d.get("content") or []) if b.get("type") == "tool_use")
    bits = [f"stop={d.get('stop_reason')}", "blocks=[" + ",".join(blocks) + "]"]
    if ntu:
        bits.append(f"tool_use x{ntu}")
    bits.append(f"in={u.get('input_tokens')} out={u.get('output_tokens')} cw={u.get('cache_creation_input_tokens')} cr={u.get('cache_read_input_tokens')}")
    print(f"  ok   {n:<{w}} {code:>4}  " + " ".join(bits))
print()
PY
