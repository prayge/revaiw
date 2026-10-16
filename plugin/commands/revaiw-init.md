---
description: Set up revaiw (local PR reviews) in this repo
---
Set up revaiw in this repository:

1. Run `revaiw init` from the repo root. If `revaiw` is not installed, install it first with
   `cargo install --git https://github.com/prayge/revaiw revaiw`. That needs the Rust toolchain; if `cargo` is
   missing, tell the user to install it from https://rustup.rs and stop.
   If `revaiw` is installed but the shell says "command not found", `~/.cargo/bin` is not on PATH: run it as
   `~/.cargo/bin/revaiw init` and give the user the PATH line `revaiw init` prints.
2. Show the user the prerequisite lines `revaiw init` printed, and for each ✗ give its install hint. Do not install
   system packages yourself.
3. Ask the user, one question at a time with a recommended answer, what this repo needs in `.review/config.toml`:
   the base branch, which paths are risky, which project skills match which paths, whether `@review fix` may push
   (`[fix] enabled`), and whether any checks should run. Edit the file accordingly.
4. Ask what this repo's reviewer must enforce, and fill in `REVIEW.md` §3 with those rules.
5. Tell the user to commit both files to the default branch (revaiw reads them only from there) and run `revaiw start`.
6. Offer the statusline segment: "Want a ◆REV segment in your statusline? (/revaiw-statusline)".
