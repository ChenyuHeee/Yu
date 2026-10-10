#!/usr/bin/env bash
# Probe 3: resolve the thinking-echo-back contradiction from probes 1 and 2.
#
# probe1 tool_roundtrip  -> 400 "thinking must be passed back"  (fabricated tool_use, no thinking)
# probe2 tool_error      -> 400 same                             (fabricated tool_use, no thinking)
# probe2 replay_nothink  -> 200                                  (real tool_use, thinking stripped)
# probe2 replay_full     -> 200 in=175  ==  replay_nothink in=175  <-- suspicious
#
# Every request body is written to disk so each case is verifiable.

set -uo pipefail
BASE="${ANTHROPIC_BASE_URL:?}"
TOK="${ANTHROPIC_AUTH_TOKEN:?}"
MODEL="${ANTHROPIC_MODEL:-deepseek-flash[1m]}"
OUT="${1:-/tmp/yu-probe3}"
mkdir -p "$OUT/req"

send() { # send <name> <body>
  local name="$1" body="$2"
  printf '%s' "$body" > "$OUT/req/$name.json"
  curl -sS --max-time 90 -o "$OUT/$name.body" -w '%{http_code}' \
    -X POST "$BASE/v1/messages" \
    -H 'content-type: application/json' \
    -H "x-api-key: $TOK" \
    -H 'anthropic-version: 2023-06-01' \
    -d "$body" > "$OUT/$name.code" 2>"$OUT/$name.err" || echo 000 > "$OUT/$name.code"
}

TOOLS='[{"name":"get_time","description":"Return the current time in a named timezone.","input_schema":{"type":"object","properties":{"tz":{"type":"string"}},"required":["tz"]}}]'

# Capture a real tool-use turn, twice with the same prefix, so the second is a cache hit.
send cap1 "{\"model\":\"$MODEL\",\"max_tokens\":2000,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"}]}"
send cap2 "{\"model\":\"$MODEL\",\"max_tokens\":2000,\"tools\":$TOOLS,\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"}]}"

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
d = json.load(open(os.path.join(out, "cap2.body")))
if "error" in d:
    print("CAPTURE FAILED:", d["error"]); sys.exit(1)
asst = d["content"]
json.dump(asst, open(os.path.join(out, "asst_real.json"), "w"))
json.dump([b for b in asst if b["type"] != "thinking"], open(os.path.join(out, "asst_stripped.json"), "w"))
tu = [b for b in asst if b["type"] == "tool_use"][0]
open(os.path.join(out, "tuid.txt"), "w").write(tu["id"])
print("real id:", tu["id"], "| blocks:", [b["type"] for b in asst],
      "| thinking chars:", sum(len(b.get("thinking","")) for b in asst if b["type"]=="thinking"))
PY

TU=$(cat "$OUT/tuid.txt")
ASST_REAL=$(cat "$OUT/asst_real.json")
ASST_STRIP=$(cat "$OUT/asst_stripped.json")
USERMSG='{"role":"user","content":"What time is it in Tokyo? Use the tool."}'

wrap() { # wrap <assistant-content> <tool-result-content>
  printf '{"model":"%s","max_tokens":100,"tools":%s,"messages":[%s,{"role":"assistant","content":%s},{"role":"user","content":[{"type":"tool_result","tool_use_id":"%s","content":%s}]}]}' \
    "$MODEL" "$TOOLS" "$USERMSG" "$1" "$TU" "$2"
}

# --- same tool_use id (real), differing only in whether thinking rides along
send A_real_with_thinking  "$(wrap "$ASST_REAL"  '"14:32 JST"')"
send B_real_stripped       "$(wrap "$ASST_STRIP" '"14:32 JST"')"

# --- fabricated tool_use, same id, no thinking
FAKE_TU="[{\"type\":\"tool_use\",\"id\":\"$TU\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]"
send C_fake_same_id        "$(wrap "$FAKE_TU" '"14:32 JST"')"

# --- fabricated tool_use, model-invented id
FAKE_ID="[{\"type\":\"tool_use\",\"id\":\"toolu_probe_zzz\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]"
send D_fake_new_id         "$(wrap "$FAKE_ID" '"14:32 JST"')"

# --- fabricated tool_use with Anthropic-style id
FAKE_ANTH="[{\"type\":\"tool_use\",\"id\":\"toolu_01ABCdef\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]"
send E_fake_anthropic_id   "$(wrap "$FAKE_ANTH" '"14:32 JST"')"

# --- is_error variant, thinking present
send F_real_iserror        "$(wrap "$ASST_REAL" '{"__err":1}' | sed 's/"content":{"__err":1}/"is_error":true,"content":"Error: tz database unavailable"/')"

# --- plain text turn with no thinking block: user / assistant / user
send G_text_nothink '{"model":"'"$MODEL"'","max_tokens":100,"messages":[{"role":"user","content":"Say ok"},{"role":"assistant","content":"ok"},{"role":"user","content":"Now say done"}]}'

# --- same, but assistant carries a fabricated thinking block
send H_text_fakethinking '{"model":"'"$MODEL"'","max_tokens":100,"messages":[{"role":"user","content":"Say ok"},{"role":"assistant","content":[{"type":"thinking","thinking":"The user wants a short answer.","signature":"not-a-real-signature"},{"type":"text","text":"ok"}]},{"role":"user","content":"Now say done"}]}'

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
names = ["cap1","cap2","A_real_with_thinking","B_real_stripped","C_fake_same_id",
         "D_fake_new_id","E_fake_anthropic_id","F_real_iserror","G_text_nothink","H_text_fakethinking"]
print()
print("=" * 104)
print("  PROBE 3  -  when is thinking echo-back actually required?")
print("=" * 104)
for n in names:
    try:
        code = open(os.path.join(out, n + ".code")).read().strip()
    except OSError:
        code = "???"
    try:
        raw = open(os.path.join(out, n + ".body"), "rb").read()
        d = json.loads(raw)
    except Exception as e:
        print(f"  ---- {n:<22} {code:>4}  unparseable: {e}")
        continue
    req = os.path.join(out, "req", n + ".json")
    rq = os.path.getsize(req) if os.path.exists(req) else 0
    if isinstance(d, dict) and "error" in d:
        msg = str(d["error"].get("message", ""))[:74]
        print(f"  FAIL {n:<22} {code:>4} (req {rq:>5}B)  {msg}")
        continue
    u = d.get("usage") or {}
    print(f"  ok   {n:<22} {code:>4} (req {rq:>5}B)  stop={d.get('stop_reason'):<10} "
          f"in={u.get('input_tokens'):<6} out={u.get('output_tokens'):<4} cr={u.get('cache_read_input_tokens')}")
print()
# Prove the two request bodies genuinely differ.
a = json.load(open(os.path.join(out, "req", "A_real_with_thinking.json")))
b = json.load(open(os.path.join(out, "req", "B_real_stripped.json")))
print("A vs B assistant content identical? ", a["messages"][1] == b["messages"][1])
print("A assistant blocks:", [x["type"] for x in a["messages"][1]["content"]])
print("B assistant blocks:", [x["type"] for x in b["messages"][1]["content"]])
PY
