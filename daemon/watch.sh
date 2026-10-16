#!/usr/bin/env bash
# Event stream of revaiw results for one branch, meant for Claude's Monitor tool: every stdout line
# becomes a notification in the conversation that armed it, so each conversation watches its own branch.
#   revaiw watch [branch]      (default: the current branch; any worktree)
# Lines: "review <b>@<sha>: 🔴 N important …" · "review …: clean …, waiting for CI" · "review …: ⚠️ partial …"
#        "ci <b>@<sha>: 🧪 …" · "fix <b>@<sha>: 🔧 pushed <ids> → <sha>" · "fix <b>@<sha>: ⛔ not pushed (<reason>)" · "<b>: ⚠️ partial review @<sha> …"
#        "<b>: ✅ ready to merge @<sha> …" · "<b>: CI green …" · "<b>: … failed …" · "revaiw stopped"
# Exits at once if the daemon isn't running, and when it is stopped (`revaiw stop` leaves a `stopped` marker).
# A restart (`revaiw start` replacing a stale build, a crash that plugin/hooks/autostart.sh recovers) is not a stop: the watch
# waits up to WATCH_RESTART_WAIT seconds (default 120) for the new daemon and keeps going.
# @see README.md
set -uo pipefail
branch="${1:-$(git branch --show-current)}"
dir="$(git rev-parse --path-format=absolute --git-common-dir)/revaiw"
live() { local p; p="$(cat "$dir/daemon.pid" 2>/dev/null)"; [ -n "$p" ] && kill -0 "$p" 2>/dev/null && echo "$p"; }
pid="$(live)"
if [ -z "$pid" ]; then
  echo "revaiw not running (start it with: revaiw start); not watching $branch"
  exit 0
fi
while [ -n "$pid" ]; do
  # tail --pid ends this daemon's stream when it exits; fixed-string matches, since branch names contain '/'.
  tail -n0 -F --pid="$pid" "$dir/daemon.log" 2>/dev/null \
    | grep --line-buffered -F -e "] review $branch@" -e "] ci $branch@" -e "] fix $branch@" -e "] $branch: " \
    | sed -u 's/^\[[0-9]*\] //'
  pid=""
  for _ in $(seq "${WATCH_RESTART_WAIT:-120}"); do
    [ -f "$dir/stopped" ] && break
    pid="$(live)" && break
    sleep 1
  done
done
echo "revaiw stopped; no longer watching $branch"
