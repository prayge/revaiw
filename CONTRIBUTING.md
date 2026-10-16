# Contributing to revaiw

Thanks for helping. revaiw is a small MVP maintained by one person, tested on one setup (see README →
Prerequisites), so changes are reviewed carefully and merged by the maintainer only.

## How to contribute

1. **Open an issue first** for anything bigger than a typo or an obvious bug fix, so we agree on the approach before
   you write code.
2. **Fork** the repo and create a branch from `main`.
3. **Make the change with tests.** Every behaviour change comes with a test that fails without it (Rust unit tests in
   `daemon/src/*.rs`, shell tests in `*.test.sh`).
4. **Run the checks** — all must pass:
   ```bash
   bash scripts/verify.sh            # unit suite, plugin test, end-to-end test (fake claude/gh)
   ```
   They cap their own resource use (`timeout`, `cargo -j 2`).
5. **Open a pull request** against `main` using the PR template. Keep it focused on one change.

`main` is protected: changes land only through pull requests that the maintainer has reviewed; force-pushes and
merge commits are not allowed.

## What to keep in mind

- **Security model first.** revaiw runs a coding agent on pull-request content. Read README → "Security model"
  before touching `daemon/src/fix.rs`, `git.rs`, `review.rs` (sandbox settings), `config.rs` (trust of
  `.review/config.toml`) or `inbox.rs` (who may trigger what). Changes that widen what the engine can read, write or
  run need a clear justification and tests.
- **Nothing repo-specific in `daemon/` or `plugin/`.** A branch name, path or rule that only fits one repo belongs in
  that repo's `.review/config.toml` or `REVIEW.md`.
- **Keep it small.** No new crates without discussion.
- **Report vulnerabilities privately** — see [SECURITY.md](SECURITY.md), never in a public issue.

## Licence

By contributing you agree that your contributions are licensed under the [MIT Licence](LICENSE).
