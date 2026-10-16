#!/usr/bin/env bash
# Applies .github/rulesets/protect-main.json to the GitHub repo (creates it, or updates the existing one by name).
# What it enforces on the default branch: changes only through a pull request that the code owner (@prayge) has
# approved, review threads resolved, squash or rebase merges only (linear history), no force-pushes, no deletion.
# The repository-admin role (the maintainer) can bypass, so only the maintainer can merge without a review.
#   bash scripts/apply-ruleset.sh            # default repo: prayge/revaiw
#   bash scripts/apply-ruleset.sh owner/repo
# GitHub enforces rulesets on public repos, or on private repos with a paid plan; otherwise the API answers 403.
set -euo pipefail
repo="${1:-prayge/revaiw}"
file="$(cd "$(dirname "$0")/.." && pwd)/.github/rulesets/protect-main.json"
id="$(gh api "repos/$repo/rulesets" --jq '.[] | select(.name=="protect-main") | .id' 2>/dev/null || true)"
if [ -n "$id" ]; then
  gh api -X PUT "repos/$repo/rulesets/$id" --input "$file" --jq '"updated ruleset \(.name) (#\(.id)): \(.enforcement)"'
else
  gh api -X POST "repos/$repo/rulesets" --input "$file" --jq '"created ruleset \(.name) (#\(.id)): \(.enforcement)"'
fi
# Merge settings that go with it: no merge commits, branches deleted after merge.
gh api -X PATCH "repos/$repo" -F allow_merge_commit=false -F allow_squash_merge=true -F allow_rebase_merge=true \
  -F delete_branch_on_merge=true --jq '"merge settings: squash=\(.allow_squash_merge) rebase=\(.allow_rebase_merge) merge-commit=\(.allow_merge_commit) delete-branch=\(.delete_branch_on_merge)"'
# Private vulnerability reporting (SECURITY.md points to it); available once the repo is public.
gh api -X PUT "repos/$repo/private-vulnerability-reporting" >/dev/null && echo "private vulnerability reporting: on"
