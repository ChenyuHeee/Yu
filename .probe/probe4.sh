#!/usr/bin/env bash
# Probe 4: is the tool_use id validated by FORMAT, or must it be one the endpoint issued?
#
# Evidence so far (all self-consistent tool_use/tool_result pairs):
#   id = call_00_AskRNGKIhPkwjTPfFmG82707 (issued)      -> 200
#   id = toolu_probe_1                    (never issued, Anthropic shape) -> 400 "thinking must be passed back"
#   id = call_probe_err                   (never issued, near-miss shape) -> 400 same
#
# M3 replay/fork needs to know whether ids can be synthesized.

set -uo pipefail
BASE="${ANTHROPIC_BASE_URL:?}"
TOK="${ANTHROPIC_AUTH_TOKEN:?}"
MODEL="${ANTHROPIC_MODEL:-deepseek-flash[1m]}"
OUT="${1:-/tmp/yu-probe4}"
mkdir -p "$OUT"

send() {
  local name="$1" body="$2"
  printf '%s' "$body" > "$OUT/$name.req"
  curl -sS --max-time 90 -o "$OUT/$name.body" -w '%{http_code}' \
    -X POST "$BASE/v1/messages" \
    -H 'content-type: application/json' \
    -H "x-api-key: $TOK" \
    -H 'anthropic-version: 2023-06-01' \
    -d "$body" > "$OUT/$name.code" 2>/dev/null || echo 000 > "$OUT/$name.code"
}

TOOLS='[{"name":"get_time","description":"Return the current time in a named timezone.","input_schema":{"type":"object","properties":{"tz":{"type":"string"}},"required":["tz"]}}]'

# Build a history with a chosen id, and a matching tool_result. No thinking block anywhere.
probe() { # probe <name> <id>
  local name="$1" id="$2"
  send "$name" "{\"model\":\"$MODEL\",\"max_tokens\":60,\"tools\":$TOOLS,\"messages\":[
    {\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"},
    {\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]},
    {\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"14:32 JST\"}]}]}"
}

# Format-valid per DeepSeek's observed shape, but certainly never issued by anyone.
probe F1_format_valid_20  "call_00_ZZZZzzzz1111AAAA9999"
probe F2_format_valid_24  "call_00_ZZZZzzzz1111AAAA9999bbbb"
probe F3_two_digits_99    "call_99_ZZZZzzzz1111AAAA9999"
probe F4_single_digit     "call_0_ZZZZzzzz1111AAAA9999"
probe F5_no_underscore    "call00ZZZZzzzz1111AAAA9999"
probe F6_short             "call_00_abc"
probe F7_anthropic_shape   "toolu_01ZZZZzzzz1111AAAA9999"

# Same, but WITH a thinking block carried on the assistant turn - does that change the verdict?
probeT() { # probeT <name> <id>
  local name="$1" id="$2"
  send "$name" "{\"model\":\"$MODEL\",\"max_tokens\":60,\"tools\":$TOOLS,\"messages\":[
    {\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"},
    {\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"I should call the time tool.\",\"signature\":\"00000000-0000-0000-0000-000000000000\"},{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]},
    {\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"14:32 JST\"}]}]}"
}

probeT T1_format_valid_with_thinking "call_00_ZZZZzzzz1111AAAA9999"
probeT T2_anthropic_shape_with_thinking "toolu_01ZZZZzzzz1111AAAA9999"

python3 - "$OUT" <<'PY'
import json, os, sys
out = sys.argv[1]
names = ["F1_format_valid_20","F2_format_valid_24","F3_two_digits_99","F4_single_digit",
         "F5_no_underscore","F6_short","F7_anthropic_shape",
         "T1_format_valid_with_thinking","T2_anthropic_shape_with_thinking"]
print()
print("=" * 100)
print("  PROBE 4  -  is tool_use.id validated by format, or must it be endpoint-issued?")
print("=" * 100)
for n in names:
    code = open(os.path.join(out, n + ".code")).read().strip()
    try:
        d = json.loads(open(os.path.join(out, n + ".body"), "rb").read())
    except Exception as e:
        print(f"  ---- {n:<34} {code:>4}  unparseable: {e}"); continue
    if isinstance(d, dict) and "error" in d:
        print(f"  FAIL {n:<34} {code:>4}  {str(d['error'].get('message',''))[:60]}")
    else:
        u = d.get("usage") or {}
        print(f"  ok   {n:<34} {code:>4}  stop={d.get('stop_reason'):<10} in={u.get('input_tokens')} out={u.get('output_tokens')}")
print()
PY
