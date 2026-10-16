# Security policy

revaiw runs Claude Code on pull-request content and can push fixes to branches, so security reports are taken
seriously.

## Reporting a vulnerability

**Do not open a public issue.** Report privately:

- Use GitHub's **"Report a vulnerability"** button on the repository's Security tab (private vulnerability
  reporting), or
- contact the maintainer, @prayge, through their GitHub profile.

Please include what you found, how to reproduce it (a minimal repo or PR content is ideal), and the impact you
expect. You should get an acknowledgement within a few days. Fixes are released as soon as practical and credited
unless you prefer otherwise.

## In scope

Anything that lets pull-request content, a PR comment, a reaction or a repo's files:

- run code outside the sandbox, or read files the sandbox should deny (credentials, other sessions' data);
- change how its own review is configured or instructed (`.review/config.toml`, `REVIEW.md`, `.claude/`);
- make revaiw push to a branch it should not, overwrite someone's push, or commit files it should not;
- trigger reviews or fixes without write permission on the repository.

## Out of scope

- `checks.commands` running a PR's own code outside the sandbox — this is documented, opt-in behaviour (README).
- Platforms other than the tested setup in README → Prerequisites.

## Supported versions

Only the latest `main` is supported while revaiw is pre-1.0.
