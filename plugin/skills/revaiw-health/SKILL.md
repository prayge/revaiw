---
name: revaiw-health
description: Use when asked whether revaiw (the local PR reviewer) is healthy or working, why reviews are partial, slow, failing or missing, for revaiw errors, logs or stats, or before trusting its verdicts after a busy day with several agents pushing.
---

# Check revaiw's health

revaiw runs one daemon per repo. Its state lives in `<git common dir>/revaiw/` (`daemon.log`, `status.json`,
`queue.json`), shared by every worktree of that repo. Two commands read all of it. Don't parse the files by hand.

## 1. Run both, from the repo in question

```
cd <any worktree of the repo>   # from another repo you get that repo's daemon
revaiw status                   # running? what is running or queued, today's totals, the last 5 results
revaiw report [hours]           # last 24 h by default: results, timings, refused commands, failures, errors
```

If `revaiw report` prints the status view instead, the installed binary is older than the repo: tell the user to
reinstall revaiw (`cargo install --path daemon` in the revaiw checkout), then rerun.

## 2. Read it

| Report line | Means | Do |
|---|---|---|
| `⚠️ partial` | The count of reviews that could not read or check something. Each partial review is counted once, and one can also have important findings. | Each refused command below explains one. If there are more partial reviews than refused commands, the engine marked the rest incomplete itself, and the review's `.md` lists what went unchecked. |
| `refused commands` | `/code-review` ran a shell script (loop, `x=…`, `$(…)`, awk, sed), and the sandbox refused it under `dontAsk`. | Not a config problem. Don't widen the sandbox or the allowlist: the engine is told to use plain commands, and widening it weakens the boundary. If this keeps happening after an upgrade, report it as a revaiw issue. `revaiw now <branch>` re-runs one review. |
| `failed` | The engine's output could not be used. The raw output is kept at the path in the line. | Quote the line. One failure: `revaiw now <branch>`. Repeated failures: raise it as a revaiw issue. |
| `errors` `inbox: error connecting…` | GitHub could not be reached. The inbox retries. | Nothing to do unless it is most of the window. |
| `errors` `comment:` / `sync PR:` | A post to GitHub failed. The review is still in `.reviews/`. | Check `gh auth status`. |
| `usage-limit pauses` | The Claude plan hit its limit, so the daemon paused and requeued the work. | Wait. Nothing was lost. |
| `resource-guard holds` | Jobs waited while the box was busy (`revaiw guard on`). | This only adds wait time. Explain it if the queue wait median is high. |

## 3. Answer

Lead with one verdict line ("healthy", or "healthy, but N partial reviews"), then only the rows that are not zero.
A clean review is not "ready to merge" until the log says `✅ ready to merge`, because CI may still be due.
Don't start, stop or restart the daemon unless the user asks.
