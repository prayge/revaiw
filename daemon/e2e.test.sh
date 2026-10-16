#!/usr/bin/env bash
# End to end: a bare remote, a clone with .review/config.toml (fixes on), the real revaiw binary, fake claude/gh.
# timeout -k 15 900 bash daemon/e2e.test.sh
# Everything lives in a tempdir except the per-checkout session dirs the daemon creates and removes under
# ~/.claude/projects/. The resource guard is off by default (`revaiw guard on`), so a busy box does not hold jobs here.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
timeout -k 15 600 cargo build -q -j 2 --manifest-path "$here/Cargo.toml"
bin="$here/target/debug/revaiw"
t=$(mktemp -d); daemon_pid=""
# The daemon leads its own process group: end the group, give it 5 s to go, then remove the tempdir. `|| true`
# throughout: under `set -e` a failed kill would end the trap before the rm.
cleanup() {
  if [ -n "$daemon_pid" ]; then
    kill -- "-$daemon_pid" 2>/dev/null || true
    for _ in $(seq 50); do kill -0 "$daemon_pid" 2>/dev/null || break; sleep 0.1; done
  fi
  rm -rf "$t"
}
trap cleanup EXIT
export FAKE_LOG="$t/calls.log" FAKE_GH="$t/gh" REVAIW_CLAUDE_BIN="$here/tests/fake/claude" REVAIW_DEBOUNCE=1
# Fix checkouts live under $XDG_CACHE_HOME/revaiw/<repo>/runs/: keep them in the tempdir, not the real ~/.cache.
export XDG_CACHE_HOME="$t/cache" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export PATH="$here/tests/fake:$PATH" GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
mkdir -p "$FAKE_GH"
git init -q --bare -b main "$t/remote.git"
# init + remote add, not clone: cloning the empty remote prints a warning on every run.
git init -q -b main "$t/clone" && cd "$t/clone" && git remote add origin "$t/remote.git"
mkdir .review && printf '[fix]\nenabled = true\n[triggers]\npoll_secs = 10\n' > .review/config.toml
echo "x" > a.ts && git add . && git commit -qm base && git push -q origin main
git remote set-head origin main
fail() { echo "FAIL: $1"; echo "--- daemon.log"; cat "$t/clone/.git/revaiw/daemon.log" 2>/dev/null || true
         echo "--- calls.log"; cat "$FAKE_LOG" 2>/dev/null || true; exit 1; }
wait_for() { for _ in $(seq 120); do grep -q "$1" "$t/clone/.git/revaiw/daemon.log" 2>/dev/null && return 0; sleep 1; done
             fail "no '$1' in daemon.log"; }
wait_call() { for _ in $(seq 120); do grep -q "$1" "$FAKE_LOG" 2>/dev/null && return 0; sleep 1; done
              fail "no call with '$1'"; }
# The daemon re-reads comments.json every poll: never let it see a half-written file.
comments() { cat > "$FAKE_GH/comments.json.tmp"; mv "$FAKE_GH/comments.json.tmp" "$FAKE_GH/comments.json"; }
remote_head() { git -C "$t/remote.git" rev-parse refs/heads/feat/x; }

"$bin" start
daemon_pid=$(cat .git/revaiw/daemon.pid)
git checkout -qb feat/x && echo "y" >> a.ts && git commit -qam change && git push -q origin feat/x
wait_for "review feat/x@.*🔴 1 important"
echo "ok push → review"

# A writer's mention with a question, and a non-writer's fix request (ignored).
now=$(date -u +%FT%TZ)
comments <<JSON
[{"id": 101, "body": "@review is n=0 handled?", "user": {"login": "kim"}, "created_at": "$now",
  "html_url": "https://github.com/o/r/pull/7#issuecomment-101", "issue_url": "https://api.github.com/repos/o/r/issues/7"},
 {"id": 102, "body": "@review fix", "user": {"login": "eve"}, "created_at": "$now",
  "html_url": "https://github.com/o/r/pull/7#issuecomment-102", "issue_url": "https://api.github.com/repos/o/r/issues/7"}]
JSON
wait_for "review requested by @kim (#101)"
wait_for "mention by eve on #7 ignored"
before=$(remote_head)
wait_call "^engine-request-seen"      # the engine got the quoted request
wait_call "^formatter-request-seen"   # and so did the formatter
# By now the poll's actions are all applied: eve's request got no 👀 (it was never queued).
! grep -q "issues/comments/102/reactions" "$FAKE_LOG" || fail "eve's ignored mention got a reaction"
echo "ok mention → review with request; non-writer ignored"

# A writer's fix request: the fake engine edits a.ts; revaiw commits and pushes to the bare remote.
[ "$(remote_head)" = "$before" ] || fail "feat/x moved before any writer asked for a fix"
comments <<JSON
[{"id": 103, "body": "@review fix", "user": {"login": "kim"}, "created_at": "$(date -u +%FT%TZ)",
  "html_url": "https://github.com/o/r/pull/7#issuecomment-103", "issue_url": "https://api.github.com/repos/o/r/issues/7"}]
JSON
wait_for "fix feat/x@.*🔧 pushed"
[ "$(remote_head)" != "$before" ] || fail "nothing pushed"
[ "$(grep -c '^fix-cwd' "$FAKE_LOG")" = 1 ] || fail "the fix engine did not run exactly once"
[ "$(grep -c "^fix-cwd $t/cache/revaiw/" "$FAKE_LOG")" = 1 ] || fail "the fix engine ran outside \$XDG_CACHE_HOME"
git -C "$t/remote.git" log -1 --format=%s feat/x | grep -q "^revaiw: fix" || fail "commit subject"
[ "$(git -C "$t/remote.git" show --name-only --format= feat/x)" = "a.ts" ] || fail "commit touched more than a.ts"
echo "ok fix → push"
"$bin" stop >/dev/null
echo "e2e ok"
