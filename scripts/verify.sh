#!/usr/bin/env bash
# Re-run every check revaiw has, one at a time (the box is shared): unit suite, plugin test, end-to-end test.
#   bash scripts/verify.sh            # all three
#   bash scripts/verify.sh unit       # just one: unit | plugin | e2e
# Exits non-zero on the first failure. Heavy steps are capped with timeout and cargo -j 2.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
want="${1:-all}"
run() { printf '\n== %s ==\n' "$1"; }
if [ "$want" = all ] || [ "$want" = unit ]; then
  run "unit suite (cargo test -j 2)"
  (cd "$root/daemon" && timeout -k 15 600 cargo test -j 2 2>&1 | tail -3)
fi
if [ "$want" = all ] || [ "$want" = plugin ]; then
  run "plugin test"
  bash "$root/plugin/tests/plugin.test.sh"
fi
if [ "$want" = all ] || [ "$want" = e2e ]; then
  run "end-to-end test (fake claude/gh)"
  timeout -k 15 900 bash "$root/daemon/e2e.test.sh" 2>&1 | tail -5
fi
printf '\nverify: all requested checks passed\n'
