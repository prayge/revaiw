#!/usr/bin/env bash
# Plugin smoke test: manifests parse, hooks point at files that exist, the REV segment reads daemon state.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
fail() { echo "FAIL: $*"; exit 1; }
for f in "$here/.claude-plugin/plugin.json" "$here/hooks/hooks.json" "$here/../.claude-plugin/marketplace.json"; do
  python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$f" || fail "bad json $f"
done
python3 - "$here" <<'PY' || fail "hook command points at a missing file"
import json, os, re, sys
root = sys.argv[1]
for ev in json.load(open(f"{root}/hooks/hooks.json"))["hooks"].values():
    for m in ev:
        for h in m["hooks"]:
            p = h["command"].replace("${CLAUDE_PLUGIN_ROOT}", root).strip('"')
            assert os.access(p, os.X_OK), p
PY
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
git -C "$t" init -q && mkdir -p "$t/.git/revaiw" "$t/.review" && touch "$t/.review/config.toml"
# Off: the server is configured but not running.
out=$(cd "$t" && printf '{"cwd":"%s"}' "$t" | bash "$here/statusline/rev-segment.sh")
[[ "$out" == *"◆REV off"*"revaiw start"* ]] || fail "off segment: $out"
# Running and idle: a live pid and a status file.
echo $$ > "$t/.git/revaiw/daemon.pid"; echo '{"running":[]}' > "$t/.git/revaiw/status.json"
out=$(printf '{"cwd":"%s"}' "$t" | bash "$here/statusline/rev-segment.sh")
[[ "$out" == *"◆REV idle"* ]] || fail "idle segment: $out"
echo '{"running":[{"branch":"feat/login","head":"abc","model":"claude-sonnet-5-5","pid":1,"started_at":0}]}' > "$t/.git/revaiw/status.json"
out=$(printf '{"cwd":"%s"}' "$t" | bash "$here/statusline/rev-segment.sh")
[[ "$out" == *"▶ login (sonnet)"* ]] || fail "running segment: $out"
# Unconfigured repo: silent.
rm -rf "$t/.review" "$t/.git/revaiw"
out=$(printf '{"cwd":"%s"}' "$t" | bash "$here/statusline/rev-segment.sh")
[ -z "$out" ] || fail "unconfigured should be silent: $out"
echo "plugin ok"
