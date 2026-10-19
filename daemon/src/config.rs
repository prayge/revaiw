//! Per-repo settings: `.review/config.toml`, read from the remote's default branch (the trunk), never from a PR,
//! so a PR cannot change how its own review is routed, what is skipped, or whether fixes may push. Every key is
//! optional; see `templates/config.toml`.
//! @see README.md
use crate::git::Repo;
use serde::Deserialize;
use std::collections::BTreeMap;

pub const HAIKU: &str = "claude-haiku-5-5";
pub const SONNET: &str = "claude-sonnet-5-5";
pub const OPUS: &str = "claude-opus-5-5";
pub const PATH: &str = ".review/config.toml";

fn strings(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub branches: Branches,
    pub ci: Ci,
    pub review: Review,
    pub routing: Routing,
    pub models: Models,
    /// Path pattern → skill the engine is pointed at when the diff touches it.
    pub skills: BTreeMap<String, String>,
    pub triggers: Triggers,
    pub fix: Fix,
    pub checks: Checks,
}

impl Config {
    /// Branches a fix must never be pushed to: the base and the CI branches and PR bases, because a fix pushed to
    /// one lands unreviewed on a trunk (a promotion PR's head is a trunk).
    pub fn is_trunk(&self, branch: &str) -> bool {
        branch == self.branches.base || self.ci.branches.iter().chain(&self.ci.pr_bases).any(|b| b == branch)
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Branches {
    /// What a branch's first review diffs against. Empty until `resolve`: the remote's default branch.
    pub base: String,
    pub skip: Vec<String>,
}
impl Default for Branches {
    fn default() -> Self { Branches { base: String::new(), skip: strings(&["HEAD", "dependabot/*"]) } }
}

/// Which heads GitHub Actions reports on. Empty until `resolve`: the base branch.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Ci {
    pub branches: Vec<String>,
    pub pr_bases: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    /// Extra paths that never count toward routing and are never reviewed (added to the built-ins).
    pub generated: Vec<String>,
    /// Let the engine run `graphify query` / `graphify explain`.
    pub graphify: bool,
}
impl Default for Review {
    fn default() -> Self { Review { generated: vec![], graphify: true } }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub opus_paths: Vec<String>,
    pub sonnet_paths: Vec<String>,
    pub risk_words: Vec<String>,
}
impl Default for Routing {
    fn default() -> Self {
        Routing {
            opus_paths: strings(&["/prisma/schema", "/migrations/", "/auth/", "/billing/", "/stripe/", "/rls"]),
            sonnet_paths: strings(&["/validations/", "/connectors/", "/integrations/", "/.github/workflows/", "/infra/"]),
            risk_words: strings(&["security", "migration", "auth", "breaking", "rls", "payment"]),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub small: String,
    pub medium: String,
    pub large: String,
}
impl Default for Models {
    fn default() -> Self { Models { small: HAIKU.into(), medium: SONNET.into(), large: OPUS.into() } }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Triggers {
    pub push: bool,
    pub mention: String,
    pub poll_secs: u64,
}
impl Default for Triggers {
    fn default() -> Self { Triggers { push: true, mention: "@review".into(), poll_secs: 60 } }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Fix {
    pub enabled: bool,
    pub max_lines: u32,
    pub model: String,
}
impl Default for Fix {
    fn default() -> Self { Fix { enabled: false, max_lines: 200, model: SONNET.into() } }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Checks {
    pub commands: Vec<String>,
    pub timeout_secs: u64,
    pub on_review: bool,
}
impl Default for Checks {
    fn default() -> Self { Checks { commands: vec![], timeout_secs: 300, on_review: false } }
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Fills what defaults to the repo: the base branch, and the CI branches (the base, when unset).
pub fn resolve(mut c: Config, default_branch: &str) -> Config {
    if c.branches.base.is_empty() { c.branches.base = default_branch.to_string(); }
    if c.ci.branches.is_empty() { c.ci.branches = vec![c.branches.base.clone()]; }
    if c.ci.pr_bases.is_empty() { c.ci.pr_bases = vec![c.branches.base.clone()]; }
    c
}

/// What `load` found. `trunk_ok` false: the trunk itself could not be read (no `<remote>/HEAD`, its branch gone, or
/// git failed), so `cfg` is not the repo's: the daemon keeps its last good config, and `err` says how to fix it.
pub struct Loaded {
    pub cfg: Config,
    pub err: Option<String>,
    pub trunk_ok: bool,
}

/// The config on the remote's default branch (`<remote>/HEAD`), resolved. A missing file gives the defaults; an
/// invalid one gives the defaults plus the reason, for one log line. The defaults are the safe set: fixes off, no
/// checks. A trunk that cannot be read is not a missing file (see `Loaded`).
pub fn load(repo: &Repo) -> Loaded {
    let r = &repo.remote;
    let unknown = |trunk: &str, err: String| Loaded { cfg: resolve(Config::default(), trunk), err: Some(err), trunk_ok: false };
    let unset = |trunk: &str| unknown(trunk, format!("revaiw: {r}/HEAD is not set (or {r}/{trunk} missing); run \
                                                       `git remote set-head {r} -a`"));
    let Some(trunk) = repo.default_branch() else { return unset("main") };
    let tip = format!("{r}/{trunk}");
    if repo.git(&["rev-parse", "--verify", "--quiet", &format!("{tip}^{{commit}}")]).is_none() { return unset(&trunk); }
    let ok = |cfg, err| Loaded { cfg, err, trunk_ok: true };
    match repo.git(&["show", &format!("{tip}:{PATH}")]).map(|t| parse(&t)) {
        Some(Ok(c)) => ok(resolve(c, &trunk), None),
        Some(Err(e)) => ok(resolve(Config::default(), &trunk), Some(format!("{PATH} on {trunk}: {e}"))),
        // Not there, or git failed to read it: only an empty listing says the file is missing.
        None => match repo.git(&["ls-tree", "--name-only", &tip, "--", PATH]) {
            Some(l) if l.is_empty() => ok(resolve(Config::default(), &trunk), None),
            _ => unknown(&trunk, format!("git could not read {PATH} on {tip}")),
        },
    }
}

/// An exact branch name, `*` for any, or `prefix/*` for every branch under `prefix/`.
pub fn branch_matches(pattern: &str, branch: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => branch.starts_with(&format!("{prefix}/")),
        None => pattern == "*" || pattern == branch,
    }
}

/// A path pattern matches when `"/" + path` contains it: `/auth/` is any `auth` directory, `/apps/` the root one.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    format!("/{path}").contains(pattern)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(d: &std::path::Path, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(d).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A repo whose remote `origin` has `trunk` as its default branch, with `config` committed on it if given.
    fn repo_with(trunk: &str, config: Option<&str>) -> (tempfile::TempDir, Repo) {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", trunk]);
        sh(d, &["commit", "-q", "--allow-empty", "-m", "root"]);
        if let Some(c) = config {
            std::fs::create_dir_all(d.join(".review")).unwrap();
            std::fs::write(d.join(PATH), c).unwrap();
            sh(d, &["add", PATH]);
            sh(d, &["commit", "-q", "-m", "config"]);
        }
        sh(d, &["remote", "add", "origin", "/nonexistent"]);
        let head = sh(d, &["rev-parse", "HEAD"]);
        sh(d, &["update-ref", &format!("refs/remotes/origin/{trunk}"), &head]);
        sh(d, &["symbolic-ref", "refs/remotes/origin/HEAD", &format!("refs/remotes/origin/{trunk}")]);
        let repo = Repo::discover(d).unwrap();
        (t, repo)
    }

    #[test] fn defaults_are_the_safe_set() {
        let c = resolve(Config::default(), "main");
        assert_eq!(c.branches.base, "main");
        assert_eq!((c.ci.branches.clone(), c.ci.pr_bases.clone()), (vec!["main".to_string()], vec!["main".to_string()]));
        assert!(!c.fix.enabled, "fixes are opt-in");
        assert!(c.checks.commands.is_empty() && !c.checks.on_review);
        assert_eq!((c.triggers.mention.as_str(), c.triggers.poll_secs, c.triggers.push), ("@review", 60, true));
        assert_eq!((c.models.small.as_str(), c.models.medium.as_str(), c.models.large.as_str()), (HAIKU, SONNET, OPUS));
        assert_eq!(c.branches.skip, vec!["HEAD", "dependabot/*"]);
    }
    #[test] fn a_full_file_parses() {
        let c = parse(r#"
            [branches]
            base = "uat"
            skip = ["main"]
            [ci]
            branches = ["master"]
            pr_bases = ["master"]
            [review]
            generated = ["/fixtures/"]
            graphify = false
            [routing]
            opus_paths = ["/rls/"]
            sonnet_paths = ["/infra/"]
            risk_words = ["payment"]
            [models]
            small = "a"
            medium = "b"
            large = "c"
            [skills]
            "/packages/database/" = "prisma-patterns"
            [triggers]
            push = false
            mention = "@bot"
            poll_secs = 30
            [fix]
            enabled = true
            max_lines = 50
            model = "m"
            [checks]
            commands = ["pnpm lint"]
            timeout_secs = 120
            on_review = true
        "#).unwrap();
        let c = resolve(c, "main");
        assert_eq!(c.branches.base, "uat");
        assert_eq!(c.ci.branches, vec!["master"]);
        assert_eq!(c.skills.get("/packages/database/").map(String::as_str), Some("prisma-patterns"));
        assert!(c.fix.enabled && c.checks.on_review && !c.triggers.push && !c.review.graphify);
        assert_eq!((c.fix.max_lines, c.checks.timeout_secs, c.triggers.poll_secs), (50, 120, 30));
        assert_eq!(c.triggers.mention, "@bot");
    }
    #[test] fn a_partial_file_keeps_the_other_defaults() {
        let c = resolve(parse("[fix]\nenabled = true\n").unwrap(), "main");
        assert!(c.fix.enabled);
        assert_eq!(c.fix.max_lines, 200);
        assert_eq!(c.branches.skip, vec!["HEAD", "dependabot/*"]);
    }
    #[test] fn a_typo_is_an_error_not_a_silent_default() {
        assert!(parse("[fix]\nenable = true\n").unwrap_err().contains("enable"));
        assert!(parse("[fixes]\n").is_err());
    }
    #[test] fn trunks_are_the_base_the_ci_branches_and_the_pr_bases() {
        let c = resolve(parse("[branches]\nbase = \"uat\"\n[ci]\nbranches = [\"master\"]\npr_bases = [\"prod\"]\n").unwrap(), "main");
        for b in ["uat", "master", "prod"] { assert!(c.is_trunk(b), "{b}"); }
        assert!(!c.is_trunk("main"), "the repo default is only a trunk when the config names it");
        assert!(!c.is_trunk("feat/x") && !c.is_trunk("uat/x"));
        assert!(resolve(Config::default(), "main").is_trunk("main"), "unset, the base is the repo default");
    }
    #[test] fn branch_patterns() {
        assert!(branch_matches("dependabot/*", "dependabot/npm/x"));
        assert!(!branch_matches("dependabot/*", "feat/dependabot-config"));
        assert!(!branch_matches("dependabot/*", "dependabot"));
        assert!(branch_matches("main", "main") && !branch_matches("main", "main2"));
        assert!(branch_matches("*", "anything"));
    }
    #[test] fn path_patterns_match_from_the_root() {
        assert!(path_matches("/auth/", "apps/api/src/auth/session.ts"));
        assert!(path_matches("/apps/", "apps/x.ts"));
        assert!(!path_matches("/auth/", "apps/author.ts"));
    }
    #[test] fn load_reads_the_trunk_on_the_remote() {
        let (t, repo) = repo_with("main", Some("[branches]\nbase = \"develop\"\n"));
        // A different config committed on HEAD, while the remote trunk ref stays at the first commit, is never read.
        std::fs::write(t.path().join(PATH), "[branches]\nbase = \"evil\"\n").unwrap();
        sh(t.path(), &["add", PATH]);
        sh(t.path(), &["commit", "-q", "-m", "evil"]);
        let l = load(&repo);
        assert_eq!((l.cfg.branches.base.as_str(), l.err, l.trunk_ok), ("develop", None, true));
    }
    #[test] fn load_without_a_file_uses_defaults_on_the_default_branch() {
        let (_t, repo) = repo_with("trunk", None);
        let l = load(&repo);
        assert_eq!((l.cfg.branches.base.as_str(), l.err, l.trunk_ok), ("trunk", None, true));
    }
    #[test] fn load_with_a_bad_file_says_why_and_uses_defaults() {
        let (_t, repo) = repo_with("main", Some("[fix\n"));
        let l = load(&repo);
        assert!(!l.cfg.fix.enabled && l.trunk_ok);
        assert!(l.err.unwrap().contains(".review/config.toml on main"));
    }
    #[test] fn an_unset_remote_head_is_not_a_missing_file() {
        // `git clone` sets origin/HEAD; `git init` + `remote add` + fetch does not. Without it the trunk is unknown.
        let (t, repo) = repo_with("master", Some("[fix]\nenabled = true\n"));
        sh(t.path(), &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        let l = load(&repo);
        assert!(!l.trunk_ok);
        let e = l.err.unwrap();
        assert!(e.contains("origin/HEAD is not set") && e.contains("git remote set-head origin -a"), "{e}");
        // origin/HEAD naming a branch the remote no longer has: the same.
        sh(t.path(), &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/gone"]);
        let l = load(&repo);
        assert!(!l.trunk_ok && l.err.unwrap().contains("set-head"));
    }
}
