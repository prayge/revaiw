#!/usr/bin/env bash
# SessionStart: in a repo with .review/config.toml, keep revaiw running. Starts it in the background in the posting
# mode it last had, unless it was stopped on purpose (`revaiw stop` leaves `stopped`). Silent elsewhere.
ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null)}"
[ -n "$ROOT" ] && [ -f "$ROOT/.review/config.toml" ] || exit 0
RSD="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)/revaiw"
say() { python3 -c 'import json,sys; print(json.dumps({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":sys.argv[1]}}))' "$1"; }
pid="$(cat "$RSD/daemon.pid" 2>/dev/null)"
# A recorded pid can be reused: it counts only while it is a revaiw `__daemon` (same check as `revaiw status`).
[ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && tr '\0' '\n' <"/proc/$pid/cmdline" 2>/dev/null | grep -qx -- __daemon && exit 0
if [ -f "$RSD/stopped" ]; then say "revaiw is stopped (deliberately, via \`revaiw stop\`), so this session did not start it. \`revaiw start\` resumes it."; exit 0; fi
# `cargo install` puts revaiw in ~/.cargo/bin, which is often missing from the PATH hooks run with.
REVAIW="$(command -v revaiw || true)"
[ -z "$REVAIW" ] && [ -x "$HOME/.cargo/bin/revaiw" ] && REVAIW="$HOME/.cargo/bin/revaiw"
if [ -z "$REVAIW" ]; then
  say "This repo is set up for revaiw (.review/config.toml) but the \`revaiw\` binary is not on PATH. Install it with \`cargo install --git https://github.com/prayge/revaiw revaiw\` (needs the Rust toolchain), or run /revaiw-init."
  exit 0
fi
mkdir -p "$RSD"
NO_POST=""; [ -f "$RSD/no-post" ] && NO_POST=1
(cd "$ROOT" && REVAIW_NO_POST="$NO_POST" nohup "$REVAIW" start>>"$RSD/autostart.log" 2>&1 </dev/null &)
say "revaiw was not running, so this session started it (log: $RSD/autostart.log). The statusline shows ◆REV once it is up, if you added the segment (/revaiw-statusline)."
