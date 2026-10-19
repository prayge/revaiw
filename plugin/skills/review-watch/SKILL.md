---
name: review-watch
description: Use when the review-notices hook says revaiw will review a push you just made, after pushing a branch that has (or will get) a PR while revaiw is running, or when asked to "watch the review", "wait for the reviewer" or "babysit the PR". It arms a Monitor on revaiw's results for this conversation's branch, fixes what it reports, and says when the PR is ready to merge.
---

# Watch revaiw for this branch

revaiw (see README.md in the revaiw repo) reviews every push and diagnoses failed CI.
This skill brings its verdicts into **this** conversation, so the user never has to paste them. Each
conversation watches only its own branch.

## 1. Arm the watch

You usually don't decide this yourself: after a `git push` or `gh pr create`, the `review-notices.sh` hook
tells you the daemon is up and to arm this. Arm it once per branch per conversation.

Run from the branch's worktree:

```
Monitor  command: revaiw watch <branch>
         description: revaiw verdicts for <branch>
         timeout_ms: 1800000
```

- **Daemon not running:** the first line says `revaiw not running` and the watch ends. Tell the user
  once (`revaiw start`) and carry on without it. Not reviewing is not an error.
- **Monitor expires:** it lasts at most 30 min. Re-arm it while a verdict is still pending: after a push, or
  after a clean review that is waiting for CI.
- **Stop it** (TaskStop) when the PR is merged or closed, or when the user says to.

## 2. Act on each line

`review` and `ci` lines, and the CI verdicts (`✅ ready to merge`, `CI green`), carry the PR after the commit when the branch has one open (`review <b>@<sha> #42: …`, `<b>: ✅ ready to merge @<sha> #42 …`).

| Line | Do |
|---|---|
| `review <b>@<sha>: 🔴 N important … → <md>` | Read `<md>`. For each important finding: if it has a `suggested_patch` in the ledger (`.reviews/<b>/ledger.json` in the main worktree, absolute path in the line), apply it with `git apply` from this worktree's root. Otherwise fix it by hand. If it is wrong, mark it `dismissed` with a one-line `decision_reason` in the ledger. Then run the area's tests, commit and push the branch. The next review checks the fix. |
| `ci <b>@<sha>: 🧪 N important …` | Same as above for the CI findings. `pre_existing` ones are already broken on the base branch: mention them to the user, don't fix them here. |
| `review …: clean (0 important …), waiting for CI` | Nothing to do. Keep the watch armed. |
| `<b>: only merges <ref> @<sha>, already reviewed there; skipped` | Nothing to review: the merged code was reviewed on `<ref>`. Keep the watch armed for the CI verdict on that commit. |
| `review …: ⚠️ partial …` / `<b>: ⚠️ partial review @<sha>, …` / `<b>: CI green …, review partial …` | Not ready to merge: the reviewer could not read or check something. Tell the user what went unread (listed in `<md>` and the review comment) and offer your pre-PR review skill, if the repo has one. |
| `<b>: CI skipped @<sha> (draft PR?) …` | Draft PRs skip CI, so it isn't ready yet. Tell the user once that review is clean and CI runs when the PR is marked ready. Keep the watch armed (re-arm on expiry): the daemon keeps polling that commit for up to 24 h. |
| `<b>: ✅ ready to merge @<sha> …` | Tell the user the PR is ready to merge (review clean, CI green), and send a PushNotification. Nits stay as suggestions. |
| `<b>: CI green @<sha>, N important still open` | Fix the open importants as in the first row. |
| `<b>: … failed …`, `usage limit`, `no CI result …` | Tell the user in one line. Don't retry in a loop (`revaiw now <b>` re-runs a review once). |
| `fix <b>@<sha>: 🔧 pushed <ids> → <sha>` | revaiw pushed a requested fix. `git pull` before your next change; the next review verifies it. |
| `fix <b>@<sha>: ⛔ not pushed (<reason>)` | The requested fix was not pushed. Tell the user the reason in one line; the PR comment has the patch. |
| `revaiw stopped …` | Someone stopped the daemon on purpose (`revaiw stop`), so the watch has ended. A restart (a rebuild after a merge, a crash the autostart recovers) does not end it. Tell the user; don't restart the daemon yourself. |

## Limits

- **Round cap:** at most **3 fix-and-push rounds** per PR without the user. If importants keep coming back,
  stop and show the user the remaining findings. Follow-up reviews report only important findings, so a clean
  branch converges.
- **Branch scope:** push fixes only to this conversation's own branch. Never push to the repo's base branch (`branches.base` in `.review/config.toml`) unless the user says so.
- **Nits:** don't act on them unless the user asks; they stay in the ledger for your pre-PR review pass, if the repo has one.
