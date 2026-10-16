#!/usr/bin/env bash
# Statusline segment: revaiw's state for the session's repo. Reads the statusline JSON on stdin and prints one
# fragment, no newline: "◆REV ▶ <branch> (<model>) · N queued", "◆REV idle", or a grey "◆REV off revaiw start"
# when the repo is configured but the daemon is down. Silent in repos without .review/config.toml.
# Add to your statusline command: $(bash <plugin>/statusline/rev-segment.sh <<<"$input") — /revaiw-statusline does it.
IN=$(cat)
CWD=$(python3 -c 'import json,sys; d=json.loads(sys.stdin.read() or "{}"); print((d.get("workspace") or {}).get("project_dir") or d.get("cwd") or "")' <<<"$IN" 2>/dev/null)
CWD="${CWD:-$PWD}"
RSD="$(git -C "$CWD" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)/revaiw"
ROOT="$(git -C "$CWD" rev-parse --show-toplevel 2>/dev/null)"
pid="$(cat "$RSD/daemon.pid" 2>/dev/null)"
if [ -f "$RSD/status.json" ] && [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  python3 - "$RSD/status.json" "$RSD/queue.json" <<'PY' 2>/dev/null
import json, re, sys
s = json.load(open(sys.argv[1]))
try: q = json.load(open(sys.argv[2])); queued = len(q.get("entries", {})) + len(q.get("fixes", {}))
except Exception: queued = 0
def short(b):
    b = b.split("/", 1)[1] if "/" in b else b
    return b if len(b) <= 26 else b[:25] + "…"
# "claude-sonnet-5-5" -> "sonnet"; "ci:claude-…" -> "ci sonnet"; "claude-haiku-5-5 · format" -> "haiku · format"; "fix · checks" as is.
def model(m):
    m = (m or "").replace("ci:", "ci ").replace("fix:", "fix ").replace("claude-", "")
    return re.sub(r"(-\d+)+(?=\s|$)", "", m)
run = s.get("running", [])
parts = ["▶ " + ", ".join("%s (%s)" % (short(r.get("branch") or "?"), model(r.get("model"))) for r in run)] if run else ["idle"]
if queued: parts.append("%d queued" % queued)
print("\033[38;5;117m◆REV " + " · ".join(parts) + "\033[0m", end="")
PY
elif [ -n "$ROOT" ] && [ -f "$ROOT/.review/config.toml" ]; then
  printf '\033[38;5;244m◆REV off\033[0m \033[38;5;108mrevaiw start\033[0m'
fi
