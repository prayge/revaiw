# Review standard

What a reviewer checks in this repo. revaiw passes this file to every review run, and Claude's `/code-review`
reads it too. Every rule below should cite the code that enforces it. If a rule and the code disagree, the code
wins and the rule is a finding (stale doc).

## 1. How to review

1. **Orient with the graph before reading source.** `graphify query "<question>"` and
   `graphify explain "<symbol>"`, 4–5 hops. "Who uses X" is answered by the graph, not grep: grep only finds
   names spelled exactly, and misses re-exports, renamed imports and docs that describe a concept in other
   words.
2. **Open every changed file at the reviewed commit**, not just its diff hunk.
3. **Trace beyond the changed lines.** For every changed function, read the unchanged code it calls and that
   calls it. Most real bugs are a new line meeting an old one: a coalescing rule, a cache, a shortcut, an early
   return, a state machine. Check what an enum value or status actually means in its state machine before
   reasoning about it.
4. **Try to break it.** Don't restate what the PR says it does. For each changed behaviour, build a concrete
   failure scenario: the input, state or sequence (two calls racing, a call while one is in flight, a retry, an
   empty or missing value, an older or newer peer reading the same file or row, a deploy mid-flight) and the
   wrong result. Keep only scenarios you can walk through in the code.
5. **Verify before reporting.** Re-read the cited lines at the reviewed commit; drop anything you cannot
   confirm. Before calling a cleanup idiom a leak, read the library's teardown. A wrong finding costs the
   author a round trip.
6. **Cite `path:line` at the reviewed commit** for every behaviour claim.

## 2. Severity

**Important**: a reasonable person using the software gets a wrong result, from a trigger the finding names that is
reachable from real inputs on a path that is live today.
- Incorrect logic, a crash, data loss, or a race on a realistic input.
- Any repo gate in §3 broken.
- Secrets or PII in logs or error messages.

**Nit**, at most 5 per review (the rest as "plus N similar" in the summary):
- House conventions, comments that explain *what* not *why*, and a doc the change made untrue.
- Defensive checks, unreachable or dormant paths (an unused plan, an unset field, a disabled feature, input the
  provider cannot send) and failures that need an unlikely chain of events.

**Do not report**: lint, formatting or type errors (the toolchain already fails them), and the contents of
generated output (a stale or hand-edited generated file *is* reported).

**Follow-up pushes**: while earlier findings are still open, report important findings only. A push after a
clean review is new work and gets a full review.

## 3. Repo gates

List each rule a reviewer must enforce here, citing the code that enforces it.
