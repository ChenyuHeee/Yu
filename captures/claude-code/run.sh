#!/usr/bin/env bash
# The capture run, in one place so it is reproducible.
#
# Starts the recording proxy, points Claude Code at it, drives one tool-using
# turn, stops the proxy. The credential is never named in this file: both the
# proxy and `claude` read it from the environment, and the proxy replaces it
# with `****` at the recording point (see ../proxy.py).
#
# Run from anywhere; writes only under this directory and the gitignored
# .yu/ (Claude Code's config dir for the run).

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# OUT and CFG can be pointed elsewhere to take a variant run without touching
# the committed capture -- e.g. the TodoWrite surface, which Claude Code uses
# when CLAUDE_CODE_ENABLE_TASKS=0 (see README):
#   OUT=$PWD/.yu/raw-tasks-disabled CLAUDE_CODE_ENABLE_TASKS=0 bash run.sh
OUT="${OUT:-$HERE/raw}"
CFG="${CFG:-$ROOT/.yu/cc-config}"

: "${ANTHROPIC_BASE_URL:?the endpoint to forward to must be in the environment}"
: "${ANTHROPIC_AUTH_TOKEN:?the credential must be in the environment}"

mkdir -p "$OUT" "$CFG"
if [ -n "$(ls -A "$OUT/requests" 2>/dev/null)" ] || [ -f "$OUT/proxy.log.jsonl" ]; then
  echo "raw/ already holds a run; refusing to mix runs" >&2
  exit 1
fi

python3 "$HERE/../proxy.py" \
  --upstream "$ANTHROPIC_BASE_URL" \
  --out "$OUT" \
  --scrub-env ANTHROPIC_AUTH_TOKEN ANTHROPIC_API_KEY \
  >"$OUT/proxy.stdout.txt" 2>"$OUT/proxy.stderr.txt" &
PROXY=$!
trap 'kill "$PROXY" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  grep -q '^LISTEN ' "$OUT/proxy.stdout.txt" 2>/dev/null && break
  sleep 0.1
done
PORT="$(sed -n 's/^LISTEN 127\.0\.0\.1:\([0-9]*\)$/\1/p' "$OUT/proxy.stdout.txt")"
if [ -z "$PORT" ]; then
  echo "proxy did not come up; see $OUT/proxy.stderr.txt" >&2
  exit 1
fi

# A fresh CLAUDE_CONFIG_DIR keeps the run off ~/.claude: no user settings,
# plugins, hooks or MCP servers load, so the tools array is Claude Code's
# built-in surface rather than this machine's customisation of it.
#
# The environment is rebuilt from scratch for the same reason: when this script
# is run from inside another Claude Code session, variables like
# CLAUDE_CODE_ENTRYPOINT, CLAUDE_EFFORT and the VSCODE_* set would make the
# child a VS Code-embedded session instead of a plain CLI one. Only the
# endpoint configuration crosses over.
ENVV=(
  "PATH=$PATH" "HOME=$HOME" "TERM=dumb"
  "CLAUDE_CONFIG_DIR=$CFG"
  "ANTHROPIC_BASE_URL=http://127.0.0.1:$PORT"
)
for v in LANG TMPDIR ANTHROPIC_AUTH_TOKEN ANTHROPIC_MODEL \
         ANTHROPIC_DEFAULT_OPUS_MODEL ANTHROPIC_DEFAULT_SONNET_MODEL \
         ANTHROPIC_DEFAULT_HAIKU_MODEL CLAUDE_CODE_ENABLE_TASKS; do
  [ -n "${!v:-}" ] && ENVV+=("$v=${!v}")
done

cd "$HERE/drive"
env -i "${ENVV[@]}" \
  claude -p "$(cat "$HERE/drive/prompt.txt")" \
    --output-format stream-json --verbose \
    >"$OUT/cc-run.stdout.jsonl" 2>"$OUT/cc-run.stderr.txt" || true

# Record what the run actually was, for the README's provenance section.
{
  echo "claude: $(env -i "PATH=$PATH" "HOME=$HOME" claude --version 2>/dev/null)"
  echo "model: ${ANTHROPIC_MODEL:-<unset>}"
  echo "args: -p <prompt.txt> --output-format stream-json --verbose"
} >"$OUT/run-identity.txt"

kill "$PROXY" 2>/dev/null || true
wait "$PROXY" 2>/dev/null || true
echo "capture in $OUT"
