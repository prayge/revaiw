#!/usr/bin/env bash
# Usage: bash daemon/watch-restart.test.sh "$PWD/daemon/watch.sh". Fakes the daemon (pid file + log) in a temp git repo.
set -u
W="$1"; T=$(mktemp -d); trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$T"' EXIT
git -C "$T" init -q; D="$T/.git/revaiw"; mkdir -p "$D"; : > "$D/daemon.log"
fake() { sleep 300 & echo $! > "$D/daemon.pid"; }
fake; p1=$(cat "$D/daemon.pid")
(cd "$T" && WATCH_RESTART_WAIT=5 bash "$W" feat/x > "$T/out" 2>&1) & w=$!
sleep 1; echo "[1] review feat/x@aaa: clean (0 important, 0 nits)" >> "$D/daemon.log"
sleep 1; kill $p1; sleep 1; fake                          # a restart: no `stopped` marker
sleep 2; echo "[2] review feat/x@bbb: 🔴 1 important, 0 nits" >> "$D/daemon.log"
sleep 2; touch "$D/stopped"; kill $(cat "$D/daemon.pid")  # a deliberate stop
for _ in $(seq 20); do kill -0 $w 2>/dev/null || break; sleep 0.5; done
fail=0
grep -q "feat/x@aaa" "$T/out" || { echo "FAIL: missed the line before the restart"; fail=1; }
grep -q "feat/x@bbb" "$T/out" || { echo "FAIL: watch did not survive the restart"; fail=1; }
kill -0 $w 2>/dev/null && { echo "FAIL: watch did not end on a deliberate stop"; fail=1; }
grep -q "revaiw stopped" "$T/out" || { echo "FAIL: no stopped line"; fail=1; }
[ $fail = 0 ] && echo PASS; cat "$T/out" | sed 's/^/  out: /'; exit $fail
