#!/usr/bin/env bash
# Plugin smoke test: manifests parse, hooks point at files that exist, the REV segment reads daemon state.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
fail() { echo "FAIL: $*"; exit 1; }
for f in "$here/.claude-plugin/plugin.json" "$here/hooks/hooks.json" "$here/../.claude-plugin/marketplace.json"; do
  python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$f" || fail "bad json $f"
done
# No pinned version: Claude Code then versions the plugin by commit, so `claude plugin update` picks up every push.
python3 -c 'import json,sys; sys.exit("version" in json.load(open(sys.argv[1])))' "$here/.claude-plugin/plugin.json" \
  || fail "plugin.json pins a version, so updates never reach installed copies"
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
# Chat hook: a finished review for this conversation's branch, with its PR when it has one.
git -C "$t" checkout -q -b feat/login
cat > "$t/.git/revaiw/status.json" <<'J'
{"running":[],"completed":[
 {"branch":"feat/login","head":"abc1234def","model":"claude-sonnet-5-5","effort":"e","reason":"r","important":1,"nits":2,"pre_existing":0,"md":"/x/1.md","finished_at":1,"pr":42},
 {"branch":"feat/login","head":"bcd2345efa","model":"claude-haiku-5-5","effort":"e","reason":"r","important":0,"nits":0,"pre_existing":0,"md":"/x/2.md","finished_at":2}]}
J
out=$(printf '{"session_id":"s1","cwd":"%s","hook_event_name":"UserPromptSubmit"}' "$t" | bash "$here/hooks/review-notices.sh")
[[ "$out" == *"review feat/login@abc1234 #42 (sonnet-5-5): 1 important, 2 nits"* ]] || fail "notice with PR: $out"
[[ "$out" == *"review feat/login@bcd2345 (haiku-5-5): 0 important"* ]] || fail "notice without PR: $out"
# Unconfigured repo: silent.
rm -rf "$t/.review" "$t/.git/revaiw"
out=$(printf '{"cwd":"%s"}' "$t" | bash "$here/statusline/rev-segment.sh")
[ -z "$out" ] || fail "unconfigured should be silent: $out"
grep -q 'at most \*\*3 fix-and-push rounds\*\*' "$here/skills/review-watch/SKILL.md" || fail "review-watch round cap is not 3"
echo "plugin ok"
