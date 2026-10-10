#!/usr/bin/env bash
# Measure what the configured Anthropic-compatible endpoint actually implements.
#
# DESIGN.md pins a large Anthropic-specific surface (betas, schema-less tools,
# adaptive thinking, effort, cache breakpoints). Against a compatibility shim
# that surface is a hypothesis, not a spec -- so measure it before writing wire code.
#
# Never echoes credentials. Extracts only status + shape fields.

set -uo pipefail

BASE="${ANTHROPIC_BASE_URL:?ANTHROPIC_BASE_URL not set}"
TOK="${ANTHROPIC_AUTH_TOKEN:?ANTHROPIC_AUTH_TOKEN not set}"
MODEL="${ANTHROPIC_MODEL:-claude-opus-5}"
VER="2023-06-01"
OUT="${1:-/tmp/yu-probe}"
mkdir -p "$OUT"

AUTH_STYLE="x-api-key"   # resolved by the auth probe below
MAXTOK=16                # probes are deliberately tiny

auth_header() {
  if [ "$AUTH_STYLE" = "bearer" ]; then
    printf 'authorization: Bearer %s' "$TOK"
  else
    printf 'x-api-key: %s' "$TOK"
  fi
}

# post <name> <path> <body> [extra-header ...]
post() {
  local name="$1" path="$2" body="$3"; shift 3
  local hdrs=""
  for h in "$@"; do hdrs="$hdrs
$h"; done
  local extra=()
  if [ -n "$hdrs" ]; then
    local line
    while IFS= read -r line; do
      [ -n "$line" ] && extra+=(-H "$line")
    done <<< "$hdrs"
  fi
  curl -sS --max-time 60 -o "$OUT/$name.body" -w '%{http_code}' \
    -X POST "$BASE$path" \
    -H 'content-type: application/json' \
    -H "$(auth_header)" \
    -H "anthropic-version: $VER" \
    ${extra[@]+"${extra[@]}"} \
    -d "$body" > "$OUT/$name.code" 2>"$OUT/$name.err" || echo "000" > "$OUT/$name.code"
}

get() {
  local name="$1" path="$2"
  curl -sS --max-time 30 -o "$OUT/$name.body" -w '%{http_code}' \
    -X GET "$BASE$path" \
    -H "$(auth_header)" \
    -H "anthropic-version: $VER" > "$OUT/$name.code" 2>"$OUT/$name.err" || echo "000" > "$OUT/$name.code"
}

U='{"role":"user","content":"Reply with the single word: ok"}'

# ---------------------------------------------------------------- auth
# Try x-api-key first, fall back to bearer if the shim wants OAuth-style auth.
post auth_xapikey /v1/messages "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"messages\":[$U]}"
if [ "$(cat "$OUT/auth_xapikey.code")" != "200" ]; then
  AUTH_STYLE="bearer"
  post auth_bearer /v1/messages "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"messages\":[$U]}"
else
  cp "$OUT/auth_xapikey.body" "$OUT/auth_bearer.body" 2>/dev/null || true
  cat "$OUT/auth_xapikey.code" > "$OUT/auth_bearer.code"
fi

# ---------------------------------------------------------------- surface
get models /v1/models

post basic /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"messages\":[$U]}"

post stream /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"stream\":true,\"messages\":[$U]}"

post thinking_adaptive /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"thinking\":{\"type\":\"adaptive\"},\"messages\":[{\"role\":\"user\",\"content\":\"What is 17*23? Think it through.\"}]}"

post thinking_summarized /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"thinking\":{\"type\":\"adaptive\",\"display\":\"summarized\"},\"messages\":[{\"role\":\"user\",\"content\":\"What is 17*23? Think it through.\"}]}"

post thinking_budget /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":4096,\"thinking\":{\"type\":\"enabled\",\"budget_tokens\":2048},\"messages\":[$U]}"

post effort /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"output_config\":{\"effort\":\"low\"},\"messages\":[$U]}"

post effort_xhigh /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"output_config\":{\"effort\":\"xhigh\"},\"messages\":[$U]}"

post cache_control /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"system\":[{\"type\":\"text\",\"text\":\"You are a terse assistant. Answer in as few words as possible, never more than five.\",\"cache_control\":{\"type\":\"ephemeral\"}}],\"messages\":[$U]}"

post sys_midconv /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"messages\":[{\"role\":\"user\",\"content\":\"Say ok\"},{\"role\":\"assistant\",\"content\":\"ok\"},{\"role\":\"user\",\"content\":\"Now say done\"},{\"role\":\"system\",\"content\":\"Answer with one word only.\"}]}"

post stop_sequences /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"stop_sequences\":[\"zzz\"],\"messages\":[$U]}"

# ---------------------------------------------------------------- tools
post tool_bash /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"tools\":[{\"type\":\"bash_20250124\",\"name\":\"bash\"}],\"messages\":[$U]}"

post tool_editor /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"tools\":[{\"type\":\"text_editor_20250728\",\"name\":\"str_replace_based_edit_tool\"}],\"messages\":[$U]}"

post tool_native /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"tools\":[{\"name\":\"get_time\",\"description\":\"Return the current time in a named timezone.\",\"input_schema\":{\"type\":\"object\",\"properties\":{\"tz\":{\"type\":\"string\"}},\"required\":[\"tz\"],\"additionalProperties\":false}}],\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"}]}"

post tool_strict /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"tools\":[{\"name\":\"get_time\",\"description\":\"Return the current time in a named timezone.\",\"strict\":true,\"input_schema\":{\"type\":\"object\",\"properties\":{\"tz\":{\"type\":\"string\"}},\"required\":[\"tz\"],\"additionalProperties\":false}}],\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo? Use the tool.\"}]}"

post tool_choice_any /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"tools\":[{\"name\":\"get_time\",\"description\":\"Return the current time.\",\"input_schema\":{\"type\":\"object\",\"properties\":{\"tz\":{\"type\":\"string\"}},\"required\":[\"tz\"]}}],\"tool_choice\":{\"type\":\"any\"},\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo?\"}]}"

post tool_search /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"tools\":[{\"type\":\"tool_search_tool_regex_20251119\",\"name\":\"tool_search_tool_regex\"},{\"name\":\"deferred_tool\",\"description\":\"A tool that is deferred.\",\"defer_loading\":true,\"input_schema\":{\"type\":\"object\",\"properties\":{}}},{\"name\":\"always_on\",\"description\":\"A tool that is always loaded.\",\"input_schema\":{\"type\":\"object\",\"properties\":{}}}],\"messages\":[$U]}"

# tool_use round trip: does a tool_result come back cleanly?
post tool_roundtrip /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"tools\":[{\"name\":\"get_time\",\"description\":\"Return the current time.\",\"input_schema\":{\"type\":\"object\",\"properties\":{\"tz\":{\"type\":\"string\"}},\"required\":[\"tz\"]}}],\"messages\":[{\"role\":\"user\",\"content\":\"What time is it in Tokyo?\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_probe_1\",\"name\":\"get_time\",\"input\":{\"tz\":\"Asia/Tokyo\"}}]},{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_probe_1\",\"content\":\"14:32 JST\"}]}]}"

# ---------------------------------------------------------------- betas
post beta_ctxmgmt /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"context_management\":{\"edits\":[{\"type\":\"clear_tool_uses_20250919\"}]},\"messages\":[$U]}" \
  'anthropic-beta: context-management-2025-06-27'

post beta_compact /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"context_management\":{\"edits\":[{\"type\":\"compact_20260112\"}]},\"messages\":[$U]}" \
  'anthropic-beta: compact-2026-01-12'

post beta_task_budget /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"output_config\":{\"effort\":\"low\",\"task_budget\":{\"type\":\"tokens\",\"total\":20000}},\"messages\":[$U]}" \
  'anthropic-beta: task-budgets-2026-03-13'

post beta_fallbacks /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"fallbacks\":\"default\",\"messages\":[$U]}" \
  'anthropic-beta: server-side-fallback-2026-07-01'

post beta_mcp /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":$MAXTOK,\"mcp_servers\":[{\"type\":\"url\",\"url\":\"https://example.com/mcp\",\"name\":\"probe\"}],\"tools\":[{\"type\":\"mcp_toolset\",\"mcp_server_name\":\"probe\"}],\"messages\":[$U]}" \
  'anthropic-beta: mcp-client-2025-11-20'

post beta_output_format /v1/messages \
  "{\"model\":\"$MODEL\",\"max_tokens\":64,\"output_config\":{\"format\":{\"type\":\"json_schema\",\"schema\":{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}},\"required\":[\"answer\"],\"additionalProperties\":false}}},\"messages\":[$U]}"

# ---------------------------------------------------------------- counting
post count_tokens /v1/messages/count_tokens \
  "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Count the tokens in this sentence, please.\"}]}"

# ---------------------------------------------------------------- report
python3 - "$OUT" <<'PY'
import json, os, sys

out = sys.argv[1]

def load(name):
    p = os.path.join(out, name + ".body")
    try:
        with open(p, "rb") as f:
            raw = f.read()
    except OSError:
        return None, "no body"
    if not raw.strip():
        return None, "empty"
    try:
        return json.loads(raw), None
    except Exception:
        return None, raw[:200].decode("utf-8", "replace").replace("\n", " ")

def code(name):
    try:
        with open(os.path.join(out, name + ".code")) as f:
            return f.read().strip()
    except OSError:
        return "???"

def blk_types(d):
    c = d.get("content")
    if isinstance(c, list):
        return [b.get("type", "?") for b in c if isinstance(b, dict)]
    return []

def detail(name, d, err):
    if err:
        return err
    if not isinstance(d, dict):
        return "non-object"
    if "error" in d and isinstance(d["error"], dict):
        e = d["error"]
        msg = str(e.get("message", ""))[:110]
        return f"ERROR {e.get('type','?')}: {msg}"
    if name == "models":
        ids = [m.get("id") for m in d.get("data", []) if isinstance(m, dict)]
        return "models=" + ",".join(str(i) for i in ids[:12])
    if name == "count_tokens":
        return f"input_tokens={d.get('input_tokens')}"
    if name == "stream":
        return stream_detail(os.path.join(out, "stream.body"))
    u = d.get("usage") or {}
    bits = [f"stop={d.get('stop_reason')}"]
    bt = blk_types(d)
    if bt:
        bits.append("blocks=[" + ",".join(bt) + "]")
    if u:
        bits.append(
            "in=%s out=%s cw=%s cr=%s"
            % (
                u.get("input_tokens"),
                u.get("output_tokens"),
                u.get("cache_creation_input_tokens"),
                u.get("cache_read_input_tokens"),
            )
        )
    if d.get("model"):
        bits.append(f"model={d['model']}")
    # Surface a tool_use call shape if present.
    for b in d.get("content") or []:
        if isinstance(b, dict) and b.get("type") == "tool_use":
            bits.append(f"tool={b.get('name')} id={b.get('id')} input={json.dumps(b.get('input'))[:60]}")
    return " ".join(bits)

def stream_detail(path):
    try:
        with open(path, "r", errors="replace") as f:
            text = f.read()
    except OSError:
        return "no stream body"
    evs, seen = [], set()
    for line in text.splitlines():
        if line.startswith("event:"):
            e = line.split(":", 1)[1].strip()
            if e not in seen:
                seen.add(e)
                evs.append(e)
    return "events=[" + ",".join(evs) + f"] bytes={len(text)}"

ORDER = [
    ("auth", ["auth_xapikey", "auth_bearer"]),
    ("surface", ["models", "basic", "stream", "thinking_adaptive", "thinking_summarized",
                 "thinking_budget", "effort", "effort_xhigh", "cache_control",
                 "sys_midconv", "stop_sequences"]),
    ("tools", ["tool_bash", "tool_editor", "tool_native", "tool_strict",
               "tool_choice_any", "tool_search", "tool_roundtrip"]),
    ("betas", ["beta_ctxmgmt", "beta_compact", "beta_task_budget", "beta_fallbacks",
               "beta_mcp", "beta_output_format"]),
    ("counting", ["count_tokens"]),
]

w = 22
print()
print("=" * 100)
print(f"  PROBE RESULTS   base={os.environ.get('ANTHROPIC_BASE_URL','?')}  model={os.environ.get('ANTHROPIC_MODEL','?')}")
print("=" * 100)
for group, names in ORDER:
    print(f"\n-- {group} " + "-" * (w + 40))
    for n in names:
        d, err = load(n)
        c = code(n)
        flag = "ok  " if c == "200" and not err and isinstance(d, dict) and "error" not in d else "FAIL"
        print(f"  {flag} {n:<{w}} {c:>4}  {detail(n, d, err)}")
print()
PY
