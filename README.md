# revaiw

A local, self-hosted pull-request reviewer for any git repo. A small Rust daemon watches your repo's branches and
reviews every push with Claude Code's own `/code-review`, headless, on your logged-in Claude CLI subscription (never
an API key), inside a fail-closed sandbox. Findings land as inline comments with suggestion blocks, tracked in a
local ledger so a branch converges instead of repeating itself. It also answers `@review` mentions and 👍/👎
reactions on PRs, and pushes a fix to the PR branch only when someone with write access asks. Everything runs on
your machine; GitHub is reached through your own `gh` login by polling, so there is no webhook, no public URL and no
GitHub App.

> **MVP.** revaiw is developed and tested on one setup (see [Prerequisites](#prerequisites)). Other setups are
> untested.

## How it compares

- **openreview** (vercel-labs): cloud-hosted, needs an API key, a GitHub App and a sandbox service.
- **Anthropic Code Review**: managed, for Team and Enterprise plans.
- **revaiw**: local, runs on your Claude subscription, keeps a findings ledger, and fixes code only on request.

## Install

```bash
cargo install --git https://github.com/prayge/revaiw revaiw    # the daemon + CLI
claude plugin marketplace add prayge/revaiw                     # the Claude Code plugin
claude plugin install revaiw@revaiw
```

`cargo install` puts `revaiw` in `~/.cargo/bin`. If your shell then says `revaiw: command not found`, that folder is
not on your `PATH` (rustup normally adds it; a fresh terminal may be enough). Add it once:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
```

`revaiw init` prints this hint too when it detects the problem, and the plugin's autostart finds
`~/.cargo/bin/revaiw` even without it.

To update later: re-run the `cargo install --git …` line (add `--force`) and `claude plugin marketplace update revaiw`.

From a clone instead (to hack on revaiw): `cargo install --path daemon` and `claude plugin marketplace add
/path/to/your/clone`, then the same `claude plugin install revaiw@revaiw`.

Then, inside the repo you want reviewed, run `/revaiw-init` in Claude Code (or `revaiw init` in a shell). It writes
`.review/config.toml` and `REVIEW.md` from `templates/` if they are absent and checks the prerequisites. Commit both
files to the repo's default branch, then `revaiw start`. The plugin starts the daemon automatically in later
sessions of a repo that has `.review/config.toml`.

**Opt-in per repo.** Installing the plugin (user scope) changes nothing on its own: in any repo without a
`.review/config.toml` its hooks exit immediately and silently. Nothing is started, polled, reviewed or posted, and
no statusline is changed. A repo opts in only by committing `.review/config.toml` to its default branch. To pause a
repo that opted in, run `revaiw stop`; the plugin respects that and does not restart it until `revaiw start`.

## Prerequisites

**Tested on; other setups are untested.** macOS and other Linux distributions are not supported yet.

| Needs | Tested version | Check / install |
|---|---|---|
| Linux Mint 22.3 | kernel 7.0.0-31 | `lsb_release -d && uname -r` |
| Rust toolchain (rustc and cargo) | 1.99.0 | `cargo --version` (install from <https://rustup.rs>) |
| Claude Code, logged in | 2.1.301 | `claude --version`, then `claude` and `/login` |
| `gh`, authenticated with `repo` scope | 2.45.0 | `gh auth status` (`gh auth login -s repo`) |
| git | 2.43.0 | `git --version` |
| bubblewrap | 0.9.0 | `bwrap --version` (`sudo apt install bubblewrap`) |
| socat | 1.8.0.0 | `socat -V` (`sudo apt install socat`) |
| Python 3 (hooks and statusline) | 3.12.3 | `python3 --version` |
| graphify (optional) | 0.9.69 | `graphify --version` |
| `notify-send` / libnotify (optional, for desktop pop-ups) | n/a | `command -v notify-send` (`sudo apt install libnotify-bin`) |

Without `bwrap` and `socat` the daemon refuses to start: the sandbox fails closed.

## Commands

Run these from inside the repo.

| Command | What it does |
|---|---|
| `revaiw start` | Starts the daemon for this repo (or restarts it if the binary or posting mode changed). |
| `revaiw stop` | Stops the daemon and cancels running reviews. |
| `revaiw status` | Shows whether it runs, what is running or queued, today's totals and recent reviews. Also the default with no argument. |
| `revaiw report [hours]` | Sums up the last 24 hours (or `hours`) of `daemon.log`: reviews by result, partial reviews, timings, CI diagnoses, failures, refused commands and GitHub errors. The plugin's `revaiw-health` skill runs it. |
| `revaiw now [--model haiku\|sonnet\|opus] [branch...]` | Queues a review of the whole branch against its base, now. Defaults to the current branch. |
| `revaiw sweep [--apply] [branch...]` | Brings open PRs' threads and comments in line with the ledger (resolves fixed threads, reopens regressed ones, hides stale comments). A dry run unless `--apply`. |
| `revaiw watch [branch]` | Prints event lines (`review ...`, `ci ...`, `fix ...`) for a branch. This is what the plugin's `review-watch` skill reads. |
| `revaiw init` | Writes `.review/config.toml` and `REVIEW.md` from the templates if absent, and checks prerequisites. |
| `revaiw notify on\|off` | Turns desktop pop-ups (`notify-send`) on or off. Off by default. Each pop-up's title names the worktree the branch is checked out in and its PR (`revaiw · indexnow-decode · #42`), so results from several agents are easy to tell apart. Pop-ups cover review results and CI verdicts (✅ ready to merge, CI green). Per machine, not per repo: the setting lives in `$XDG_STATE_HOME/revaiw` (else `~/.local/state/revaiw`). With no argument it prints the current setting. |
| `revaiw guard on\|off` | Turns the resource guard (below) on or off. Off by default. Per machine, not per repo (stored next to `notify`). With no argument it prints the current setting. |

## Resource use

The daemon runs two jobs at a time (reviews, CI diagnoses, fixes and their checks all count); `REVAIW_SLOTS=3` when
starting it allows a third, for a catch-up. That slot limit is all that holds a job back by default: **the resource
guard is off**.

`revaiw guard on` adds a check on the machine before each job starts, so a busy box is left alone: the first
slot needs 3 GiB of free memory, a one-minute load under 85% of the cores and fewer than 3 other heavy runners
(jest, tsc, builds); the second and third need 6 GiB, a load under 50% and fewer than 2 heavy runners. A job that
does not pass waits (the log says `holding: resource guard ...` once per hold) and starts when the box has room.
The numbers come from `~/.claude/hooks/resource-guard.py --json` if you have that script (it also counts the other
heavy runners); otherwise revaiw reads `/proc/meminfo` and `/proc/loadavg` itself and counts none.
`revaiw status` shows `resource guard: on|off`.

## On a PR

Mentions and reactions count only from users with `admin`, `maintain` or `write` permission on the repo. Anyone
else is ignored and logged, never answered.

- `@review <question>`: queues a full review of the branch; your text reaches the reviewer as the requester's
  question and the summary answers it. A bare `@review` just re-reviews.
- `@review --model opus <question>`: pins the model tier for that review.
- `@review fix [ids]`: queues a fix job for all open important findings, or the listed finding ids. Needs
  `fix.enabled = true`.
- 👍 or ❤️ on a revaiw inline finding queues a fix for that one finding. 👎 or 😕 dismisses it in the ledger and
  the thread is resolved at the next sync. Reactions are acted on immediately; if you reacted by mistake, revert by
  hand.

A fix job checks out the PR's current head, lets the engine edit only the files of the selected findings (within
`fix.max_lines`), runs `checks.commands`, commits as you with a `Requested-by` trailer, and pushes. The push then
triggers a normal review that verifies the fix. A fix never triggers another fix. If a guard or check fails,
nothing is pushed and revaiw comments with the reason and the patch.

Fix checkouts live under `$XDG_CACHE_HOME/revaiw/<hash>/runs/` (default `~/.cache`), because Claude Code refuses
edits under any `.git/` path. For the same reason, findings in paths Claude Code protects (`.git`, `.vscode`,
`.idea`, `.claude`, `.husky`, `.cargo`, `.devcontainer`, `.yarn`, `.mvn`, `.config/git`) cannot be auto-fixed.

## Config

Per-repo behaviour comes from `.review/config.toml`, read from the repo's default branch (the trunk), never from a
PR. Every key is optional. See [`templates/config.toml`](templates/config.toml) for the keys and their defaults:
branches and skipping, CI, generated paths, model routing, skill mapping, triggers, fix, and checks. An
unparseable file falls back to the defaults and logs one line; fixes stay off unless `fix.enabled = true`.

Two things to know about the config:

- `checks.commands` run the PR's own code **outside** the sandbox. They are opt-in through the trunk's config; only
  enable them for repos whose contributors you trust.
- The reviewer's `graphify query` / `explain` only helps if the target repo tracks `graphify-out/graph.json`.

## Statusline

`/revaiw-statusline` shows what revaiw's statusline segment prints (`◆REV idle`, `◆REV ▶ branch (sonnet)`, ...) and,
if you want it, adds it to your existing statusline command. It never replaces your statusline; plugins cannot set
one.

## Security model

- **Trust comes from the base.** `.review/config.toml`, the Claude files (`.claude/`) and `REVIEW.md` are taken from
  the trunk or the PR's base, never from the PR. A PR cannot change how its own review is routed, what is skipped or
  who may trigger fixes.
- **Mention text is data** (D8). It is quoted into the prompt as the requester's question. It never reaches flags,
  tools, sandbox settings or the command line.
- **Writers only** (D7). Mentions and reactions count only from `admin`, `maintain` or `write` users; a failed
  permission lookup counts as denied.
- **The sandbox fails closed.** Reviews and fixes run the engine under `bwrap`/`socat`; if they are missing the
  daemon will not start. The sandbox's `denyRead` covers common credential files (SSH keys, cloud and `gh` tokens,
  and similar).
- **The fix engine is confined to its worktree.** The daemon never runs git inside the engine's worktree: finding
  files are harvested into a clean checkout, which is committed and pushed from there.
- **The push guard.** A fix pushes with `--force-with-lease` pinned to the reviewed head: it lands only while the
  remote branch is still exactly that commit, so it never overwrites a newer push, never resurrects commits an
  author force-pushed away, and never recreates a deleted branch.
- **No fixes onto a trunk.** A PR whose head is the base branch or one of `ci.branches` / `ci.pr_bases` (a promotion
  PR, `uat` into `master`) never gets a fix: the push would land there unreviewed. revaiw says so and asks for the fix
  on a feature branch; 👎 dismissing still works.
- **Project checks are the exception.** `checks.commands` run unsandboxed (see Config).

## Development

```bash
export PATH="$HOME/.cargo/bin:$PATH"
(cd daemon && timeout -k 15 600 cargo test -j 2)
bash plugin/tests/plugin.test.sh
timeout -k 15 900 bash daemon/e2e.test.sh
```

The e2e test drives the real daemon against a fake `claude` and a fake `gh` (push, mention, non-writer, fix and
rejected-push flows).

## Licence

MIT. See [LICENSE](LICENSE).
