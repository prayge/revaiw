#!/usr/bin/env bash
# `run.sh start` must refuse to run on Linux without bwrap and socat, and say how to install them.
# Usage: bash daemon/run-prereqs.test.sh "$PWD/daemon/run.sh"
set -u
RUN="$1"; BASH_BIN=$(command -v bash); T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
[ "$(uname -s)" = Linux ] || { echo "SKIP: the sandbox tools are required on Linux only"; exit 0; }
BIN="$T/bin"; mkdir -p "$BIN"
for t in uname dirname git; do ln -s "$(command -v $t)" "$BIN/$t"; done
fail=0
out=$(PATH="$BIN" "$BASH_BIN" "$RUN" start 2>&1); code=$?
[ $code -ne 0 ] || { echo "FAIL: start succeeded without the sandbox tools"; fail=1; }
for want in "bwrap" "socat" "sudo apt install bubblewrap socat" "revaiw start"; do
  echo "$out" | grep -q "$want" || { echo "FAIL: message lacks '$want'"; fail=1; }
done
# With both present the sandbox check passes; the next failure (no Rust toolchain here) is a different one.
printf '#!/bin/sh\n' > "$BIN/bwrap"; cp "$BIN/bwrap" "$BIN/socat"; chmod +x "$BIN/bwrap" "$BIN/socat"
out=$(PATH="$BIN" HOME="$T" "$BASH_BIN" "$RUN" start 2>&1)
echo "$out" | grep -qi "sandbox" && { echo "FAIL: complained about the sandbox with both tools present"; fail=1; }
[ $fail = 0 ] && echo PASS
exit $fail
