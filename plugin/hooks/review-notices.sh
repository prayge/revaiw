#!/usr/bin/env bash
# Feeds revaiw (the local PR reviewer) back into Claude conversations:
#   PostToolUse after `git push` / `gh pr create` while the daemon runs: arm the review-watch skill (Monitor).
# and finished reviews to the session working that branch, once each:
#   UserPromptSubmit: a one-line summary for you.
#   PostToolUse: the open important findings for the agent, with how to resolve them, so the agent that
#   wrote the code incorporates the fixes. The daemon never edits code itself.
# Fast path: nothing runs past the mtime check unless status.json changed since this session last looked.
# @see README.md
HOOK_INPUT=$(cat)
SID=$(sed -n 's/.*"session_id" *: *"\([^"]*\)".*/\1/p' <<<"$HOOK_INPUT" | head -1)
CWD=$(sed -n 's/.*"cwd" *: *"\([^"]*\)".*/\1/p' <<<"$HOOK_INPUT" | head -1)
RSD="$(git -C "${CWD:-.}" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)/revaiw"
[ -n "$SID" ] && [ -f "$RSD/status.json" ] || exit 0

# A push (or a new PR) while the daemon runs: tell this conversation to arm its own watch, so verdicts arrive
# without anyone running another command. `revaiw start` stays the only command.
is_push() {  # run as a command (line start or after && ; |), not merely mentioned in text or a heredoc
  HOOK_INPUT="$HOOK_INPUT" python3 -c '
import json, os, re, sys
d = json.loads(os.environ["HOOK_INPUT"])
cmd = (d.get("tool_input") or {}).get("command", "") if d.get("tool_name") == "Bash" else ""
sys.exit(0 if re.search(r"(^|[;&|\n(])\s*(git(\s+-C\s+\S+)?\s+push\b|gh\s+pr\s+create\b)", cmd) else 1)'
}
if grep -Eq 'push|pr create' <<<"$HOOK_INPUT" && kill -0 "$(cat "$RSD/daemon.pid" 2>/dev/null)" 2>/dev/null && is_push; then
  MSG="revaiw is running and will review this push. Unless a review-watch Monitor is already armed for the branch you pushed, invoke the review-watch skill now: Monitor on \`revaiw watch <that branch>\`. Its verdicts then arrive here on their own."
  python3 -c 'import json,sys; print(json.dumps({"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":sys.argv[1]}}))' "$MSG"
  exit 0
fi

STAMP="$RSD/seen/$SID"
[ -f "$STAMP" ] && [ ! "$RSD/status.json" -nt "$STAMP" ] && exit 0

HOOK_INPUT="$HOOK_INPUT" RSD="$RSD" STAMP="$STAMP" python3 - <<'PY' 2>/dev/null || true
import json, os, subprocess
d = json.loads(os.environ["HOOK_INPUT"])
event = d.get("hook_event_name") or "UserPromptSubmit"
cwd, rsd, stamp = d.get("cwd") or os.getcwd(), os.environ["RSD"], os.environ["STAMP"]
branch = subprocess.run(["git", "-C", cwd, "branch", "--show-current"], capture_output=True, text=True).stdout.strip()
status = json.load(open(os.path.join(rsd, "status.json")))
seen = set(open(stamp).read().split()) if os.path.exists(stamp) else set()
fresh = [c for c in status.get("completed", []) if c["branch"] == branch and c["md"] not in seen]
os.makedirs(os.path.dirname(stamp), exist_ok=True)
with open(stamp, "a") as f:  # also refreshes the mtime used by the fast path
    f.write("".join(c["md"] + "\n" for c in fresh))
if not (branch and fresh):
    raise SystemExit
lines = [f"review {c['branch']}@{c['head'][:7]} ({c['model'].replace('claude-', '')}): "
         f"{c['important']} important, {c['nits']} nits → {c['md']}" for c in fresh]
msg = "\n".join(lines)
if event == "PostToolUse":
    ledger_path = os.path.join(os.path.dirname(fresh[-1]["md"]), "ledger.json")
    try:
        ledger = json.load(open(ledger_path))["findings"]
    except Exception:
        ledger = {}
    imp = [(k, e) for k, e in ledger.items() if e.get("status") == "open" and e.get("severity") == "important"]
    if imp:
        msg += "\n\nOpen important findings on this branch (from revaiw):\n" + "\n".join(
            f"- {k} {e['file']}:{e['line']} {e['summary']}" + (" (has suggested_patch)" if e.get("suggested_patch") else "")
            for k, e in imp)
        msg += (f"\n\nYou own this branch: fix each one (apply its suggested_patch with `git apply` when it has one) "
                f"and set \"status\": \"fixed\", or set \"dismissed\"/\"deferred\" with a one-line decision_reason, "
                f"in {ledger_path}. Details: {fresh[-1]['md']}")
out = {"hookSpecificOutput": {"hookEventName": event, "additionalContext": msg}}
if event == "UserPromptSubmit":
    out["systemMessage"] = msg
print(json.dumps(out))
PY
exit 0
