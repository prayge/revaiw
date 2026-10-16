#!/usr/bin/env bash
# Builds the revaiw binary, then runs it.
#  start  always runs `cargo build` (cargo's own freshness check: ~1 s when nothing changed), then restarts a
#         running daemon that is not executing this build: one started from another checkout's binary, or
#         from a binary rebuilt since (Linux shows its /proc/<pid>/exe as "… (deleted)"). `start` used to print
#         "already running" and leave a stale daemon serving after a branch switch or a merge.
#  other  build only when sources are newer than the binary, then run.
# @see README.md
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="$here/target/release/revaiw"
# The review engine runs /code-review in a sandbox and fails closed, so `start` needs its tools on Linux (macOS
# sandboxes with Seatbelt). Checked before the slow build; review.rs::sandbox_hint says the same.
if [ "${1:-}" = "start" ] && [ "$(uname -s)" = "Linux" ]; then
  missing=""
  for t in bwrap socat; do command -v "$t" >/dev/null 2>&1 || missing="${missing:+$missing and }$t"; done
  if [ -n "$missing" ]; then
    # printf is a builtin: the message must show even when little else is on PATH.
    printf '%s\n' \
      "revaiw: the review engine runs /code-review in a sandbox (no network, writes confined to the checkout)" \
      "and fails closed, so it will not start without bubblewrap and socat. missing: $missing" \
      "  Debian/Ubuntu/Mint:  sudo apt install bubblewrap socat" \
      "  Fedora:              sudo dnf install bubblewrap socat" \
      "  Arch:                sudo pacman -S bubblewrap socat" \
      "  macOS:               nothing to install (built in)" \
      "then: revaiw start   (README.md, Prerequisites)" >&2
    exit 1
  fi
fi
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || {
  echo "revaiw needs the Rust toolchain to build: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
}
build() { timeout -k 15 600 nice -n 10 cargo build --release --quiet --manifest-path "$here/Cargo.toml" -j 2; }
rsd="$(git -C "$here" rev-parse --path-format=absolute --git-common-dir)/revaiw"

# Intent the session-start autostart (plugin/hooks/autostart.sh) reads: a deliberate `stop`
# leaves `stopped` so it is not undone, and a `start` that actually launches a daemon records whether GitHub
# posting is off (`no-post`), so an autostart after a crash comes back the same way.
[ "${1:-}" = "stop" ] && mkdir -p "$rsd" && touch "$rsd/stopped"

if [ "${1:-}" = "start" ]; then
  mkdir -p "$rsd" && rm -f "$rsd/stopped"
  build
  # `start` cleared it above, so a marker now means a `stop` arrived during the build: honour it.
  [ -f "$rsd/stopped" ] && { echo "revaiw: stopped during the build, not starting"; exit 0; }
  pid="$(cat "$rsd/daemon.pid" 2>/dev/null || true)"
  live=0
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    if [ "$(readlink "/proc/$pid/exe" 2>/dev/null)" != "$(readlink -f "$bin")" ]; then
      echo "revaiw: the running daemon (pid $pid) is not this build — restarting it on $bin"
      "$bin" stop
    else
      live=1
      # GitHub posting is read from the environment once, at launch: a start asking for the other mode than
      # the running daemon's (recorded in `no-post`) restarts it, so the request is never silently ignored.
      want=0; [ "${REVAIW_NO_POST:-}" = 1 ] && want=1
      have=0; [ -f "$rsd/no-post" ] && have=1
      if [ "$want" != "$have" ]; then
        echo "revaiw: restarting pid $pid to turn GitHub posting $([ "$want" = 1 ] && echo off || echo on)"
        "$bin" stop
        live=0
      fi
    fi
  fi
  # Only a launch changes the posting mode; a no-op start leaves the running daemon's record alone.
  if [ "$live" = 0 ]; then
    if [ "${REVAIW_NO_POST:-}" = 1 ]; then echo 1 > "$rsd/no-post"; else rm -f "$rsd/no-post"; fi
  fi
elif [ ! -x "$bin" ] || [ -n "$(find "$here/src" "$here/Cargo.toml" "$here/prompt.md" "$here/schema.json" -newer "$bin" 2>/dev/null | head -1)" ]; then
  build
fi
exec "$bin" "$@"
