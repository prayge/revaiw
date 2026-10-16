use crate::config::{branch_matches, path_matches, Config};

/// Every human-authored branch is reviewed, trunks included (a trunk push is reviewed as old..new), except the
/// configured `branches.skip` patterns (default: `HEAD` and `dependabot/*`).
pub fn skip_branch(c: &Config, branch: &str) -> bool {
    c.branches.skip.iter().any(|p| branch_matches(p, branch))
}

/// Whether GitHub Actions will report on this branch's head: a push to one of `ci.branches`, or a branch with an
/// open PR into one of `ci.pr_bases`. Anything else has no runs for its sha and is ready on a clean review alone.
pub fn ci_expected(c: &Config, branch: &str, open_pr_base: Option<&str>) -> bool {
    c.ci.branches.iter().any(|p| branch_matches(p, branch))
        || open_pr_base.is_some_and(|b| c.ci.pr_bases.iter().any(|p| branch_matches(p, b)))
}

/// `ci_expected` after a PR lookup that can fail. A failed lookup (gh timed out, auth blip) fails closed: it might
/// be a PR into a CI base, so its CI is watched rather than the review declaring it ready.
pub fn ci_expected_after_lookup(c: &Config, branch: &str, lookup: Result<Option<&str>, ()>) -> bool {
    match lookup {
        Err(()) => true,
        Ok(base) => ci_expected(c, branch, base),
    }
}

/// Machine output, lockfiles, plans, executor scratch and revaiw's own `.reviews/`, plus the configured
/// `review.generated` patterns: never reviewed and never counted for routing.
pub fn is_generated(c: &Config, path: &str) -> bool {
    let p = format!("/{path}");
    let file = path.rsplit('/').next().unwrap_or(path);
    p.starts_with("/graphify-out/") || p.starts_with("/docs/superpowers/plans/") || p.starts_with("/.superpowers/")
        || p.starts_with("/.reviews/")
        || ["/generated/", "/build/", "/dist/"].iter().any(|d| p.contains(d))
        || file.ends_with(".lock") || file == "pnpm-lock.yaml" || file == "package-lock.json"
        || c.review.generated.iter().any(|g| path_matches(g, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{resolve, Config};
    fn dflt() -> Config { resolve(Config::default(), "main") }
    /// A uat/master flow: work lands on uat, CI runs only for master and PRs into it, `main` unused.
    fn uat_flow() -> Config {
        let mut c = resolve(Config::default(), "uat");
        c.ci.branches = vec!["master".into()];
        c.ci.pr_bases = vec!["master".into()];
        c.branches.skip.push("main".into());
        c
    }
    #[test] fn reviews_both_trunks() {
        for b in ["uat", "master"] { assert!(!skip_branch(&uat_flow(), b)); }
        for b in ["main", "HEAD"] { assert!(skip_branch(&uat_flow(), b)); }
        assert!(!skip_branch(&uat_flow(), "feat/uat-thing"));
        assert!(!skip_branch(&dflt(), "main"), "by default the trunk is reviewed too");
    }
    #[test] fn a_failed_pr_lookup_fails_closed() {
        let c = uat_flow();
        assert!(ci_expected_after_lookup(&c, "feat/x", Err(())));
        assert!(ci_expected_after_lookup(&c, "uat", Ok(Some("master"))));
        assert!(!ci_expected_after_lookup(&c, "uat", Ok(None)));
        assert!(!ci_expected_after_lookup(&c, "feat/x", Ok(Some("uat"))));
    }
    #[test] fn github_ci_follows_config() {
        let c = uat_flow();
        assert!(ci_expected(&c, "master", None));
        assert!(ci_expected(&c, "uat", Some("master")));
        assert!(!ci_expected(&c, "uat", None));
        for b in ["feat/x", "sam/misplat-1-y"] {
            assert!(!ci_expected(&c, b, Some("uat")));
            assert!(!ci_expected(&c, b, None));
        }
        // Default: CI on the trunk and on PRs into it.
        assert!(ci_expected(&dflt(), "main", None) && ci_expected(&dflt(), "feat/x", Some("main")));
        assert!(!ci_expected(&dflt(), "feat/x", None));
    }
    #[test] fn skips_machine_authored_branches() {
        assert!(skip_branch(&dflt(), "dependabot/npm_and_yarn/next-15.1"));
        assert!(!skip_branch(&dflt(), "feat/dependabot-config"));
    }
    #[test] fn generated_paths() {
        let c = dflt();
        for p in ["graphify-out/graph.json", "apps/x/generated/a.ts", "apps/shopify/easy-discount/build/i.js",
                  "packages/ui/dist/i.js", "Cargo.lock", "pnpm-lock.yaml", "package-lock.json",
                  "docs/superpowers/plans/2026-10-09-mend-evals-5.md", ".superpowers/sdd/x/section-1-report.md",
                  ".reviews/feat/x/ledger.json", ".reviews/feat/x/abc1234.md"] {
            assert!(is_generated(&c, p), "{p}");
        }
        for p in ["apps/backend/src/build.ts", "src/distance.ts", "docs/superpowers/specs/2026-10-09-x-design.md",
                  "apps/platform/persisted-operations.json"] {
            assert!(!is_generated(&c, p), "{p}");
        }
    }
    #[test] fn config_adds_generated_paths() {
        let mut c = dflt();
        c.review.generated.push("/persisted-operations".into());
        assert!(is_generated(&c, "apps/platform/persisted-operations.json"));
    }
}
