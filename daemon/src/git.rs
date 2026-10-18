use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Repo {
    pub root: PathBuf,
    pub common: PathBuf,
    pub remote: String,
}

fn run(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

/// Files that instruct a reviewer or decide how it runs: every `.claude/` folder, every `CLAUDE.md` and
/// `CLAUDE.local.md`, `REVIEW.md` and `.review/`. A review or fix checkout takes them from the base, never the PR.
pub fn is_steer(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
    p.starts_with(".claude/") || p.contains("/.claude/") || p == "REVIEW.md" || p.starts_with(".review/")
        || name == "claude.md" || name == "claude.local.md"
}

/// git in the daemon's own clean fix checkout C only. It is NOT safe in the engine's worktree U: a submodule's
/// filter driver planted there still runs (the control test `the_planted_filter_fires_when_git_runs_in_the_engine_
/// worktree` proves it), so no git ever runs in U and `fix::harvest` copies the files out instead. In C it is
/// defence in depth: the admin dir comes from the trusted side (`Repo::worktree_gitdir`) rather than the worktree's
/// `.git` pointer, no fsmonitor or hooks run (`-c` reaches child gits too), and paths are literal, never globs that
/// could widen a commit.
pub fn fix_git(wt: &Path, gitdir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(wt)
        .args(["--literal-pathspecs", "-c", "core.fsmonitor=", "-c", "core.hooksPath=/dev/null", "--git-dir"]).arg(gitdir)
        .arg("--work-tree").arg(wt);
    c
}

impl Repo {
    pub fn discover(start: &Path) -> Option<Repo> {
        let common = PathBuf::from(run(start, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
        let common = common.canonicalize().ok()?;
        let root = common.parent()?.to_path_buf();
        let remote = run(&root, &["remote"])?.lines().next()?.to_string();
        Some(Repo { root, common, remote })
    }

    pub fn git(&self, args: &[&str]) -> Option<String> {
        run(&self.root, args)
    }

    /// The branch's current head on the remote, as last fetched or pushed.
    pub fn remote_head(&self, branch: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{}/{branch}", self.remote)])
    }

    /// The worktree `branch` is checked out in (the main checkout or a linked one, e.g. an agent's), if any.
    pub fn worktree_of(&self, branch: &str) -> Option<PathBuf> {
        let want = format!("branch refs/heads/{branch}");
        self.git(&["worktree", "list", "--porcelain"])?.split("\n\n")
            .find(|w| w.lines().any(|l| l == want))
            .and_then(|w| w.lines().find_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from))
    }

    /// The remote's default branch (`refs/remotes/<remote>/HEAD`); None when that is not set.
    pub fn default_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "--short", &format!("refs/remotes/{}/HEAD", self.remote)])
            .and_then(|r| r.strip_prefix(&format!("{}/", self.remote)).map(str::to_string))
    }

    /// The fix's changes in the daemon's clean checkout C (`add_clean_worktree`, filled by `fix::harvest`) against
    /// its HEAD, as numstat. Never run on the engine's worktree: git there would run whatever the engine planted
    /// (a submodule's filter driver, say). Only `files` (the harvested ones that still exist) are added, as
    /// intent-to-add, so new files count; there is no pathless `add`, and submodules are never inspected. Steer files
    /// are dropped. `-z` keeps non-ASCII paths unquoted (`a\td\tpath\0`); a rename (`a\td\t\0old\0new\0`) reports
    /// both paths, the counts on the new one. Empty (so the guard fails it as "changed nothing") when `wt` is not a
    /// linked worktree of this repo.
    pub fn changed_in(&self, wt: &Path, files: &[String]) -> Vec<(u32, u32, String)> {
        let Some(gitdir) = self.worktree_gitdir(wt) else { return vec![] };
        // A deleted file is still in the index, and `add` fails outright on a pathspec that matches nothing.
        let present: Vec<&String> = files.iter().filter(|f| std::fs::symlink_metadata(wt.join(f)).is_ok()).collect();
        if !present.is_empty() {
            let _ = fix_git(wt, &gitdir).args(["add", "-N", "--"]).args(present).output(); // quietly
        }
        let out = fix_git(wt, &gitdir).args(["diff", "--numstat", "-z", "--ignore-submodules=all", "HEAD"]).output().ok()
            .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let (mut parts, mut changed) = (out.split('\0'), vec![]);
        while let Some(rec) = parts.next() {
            let mut p = rec.splitn(3, '\t');
            let (Some(a), Some(d), Some(path)) = (p.next(), p.next(), p.next()) else { continue };
            let (a, d) = (a.parse().unwrap_or(0), d.parse().unwrap_or(0));
            if path.is_empty() {
                let (old, new) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
                changed.push((a, d, new.to_string()));
                changed.push((0, 0, old.to_string()));
            } else {
                changed.push((a, d, path.to_string()));
            }
        }
        changed.retain(|(_, _, p)| !p.is_empty() && !is_steer(p));
        changed
    }

    /// The admin dir git keeps for the linked worktree `wt` (`<common>/worktrees/<name>`), found from the trusted
    /// side: the one whose `gitdir` file names `<wt>/.git`. Never read from `wt/.git`, which the fix engine can
    /// rewrite to point at a gitdir of its own making (with an fsmonitor or hooks the daemon would then run).
    pub fn worktree_gitdir(&self, wt: &Path) -> Option<PathBuf> {
        let want = wt.canonicalize().ok()?;
        std::fs::read_dir(self.common.join("worktrees")).ok()?.flatten().map(|e| e.path()).find(|d| {
            std::fs::read_to_string(d.join("gitdir")).is_ok_and(|g| {
                let g = Path::new(g.trim_end_matches('\n'));
                g.file_name() == Some(".git".as_ref()) && g.parent().and_then(|p| p.canonicalize().ok()) == Some(want.clone())
            })
        })
    }

    /// A plain detached checkout of `head` for the daemon's own git work on a fix (harvest, guard, checks, commit):
    /// no steer swap, and the engine never touches it.
    pub fn add_clean_worktree(&self, path: &Path, head: &str) -> bool {
        self.remove_worktree(path);
        self.ok(&["worktree", "add", "--detach", "--force", &path.to_string_lossy(), head])
    }

    /// Keeps the daemon's `.reviews/` (ledgers and review notes, written in the main checkout) out of `git status`
    /// and `git add .`: appends `/.reviews/` to `<common>/info/exclude` once, local to this clone.
    pub fn exclude_reviews(&self) -> std::io::Result<()> {
        let ex = self.common.join("info/exclude");
        let body = std::fs::read_to_string(&ex).unwrap_or_default();
        if body.lines().any(|l| l.trim() == "/.reviews/") { return Ok(()); }
        std::fs::create_dir_all(self.common.join("info"))?;
        let sep = if body.is_empty() || body.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&ex, format!("{body}{sep}/.reviews/\n"))
    }

    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.ok(&["merge-base", "--is-ancestor", a, b])
    }

    fn ok(&self, args: &[&str]) -> bool {
        Command::new("git").arg("-C").arg(&self.root).args(args)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false)
    }

    /// None when git fails, so a transient failure never reads as "every branch was deleted".
    pub fn snapshot(&self) -> Option<BTreeMap<String, String>> {
        let prefix = format!("refs/remotes/{}", self.remote);
        Some(self.git(&["for-each-ref", "--format=%(refname:lstrip=3) %(objectname)", &prefix])?
            .lines()
            .filter_map(|l| l.rsplit_once(' ').map(|(b, s)| (b.to_string(), s.to_string())))
            .collect())
    }

    /// Base of the range to review: the old head when this push fast-forwards it, else the merge-base with the
    /// configured base branch, local and remote together. Local is often ahead (unpushed), and its commits are not
    /// this branch's work. When local already contains the head (the branch is the base's own unpushed work), that
    /// merge-base would be the head itself, an empty range, so fall back to the remote's.
    pub fn range(&self, old: Option<&str>, new: &str, base: &str) -> Option<String> {
        if let Some(o) = old {
            if o != new && self.is_ancestor(o, new) {
                return Some(o.to_string());
            }
        }
        let remote_base = format!("{}/{base}", self.remote);
        self.git(&["merge-base", new, base, &remote_base])
            .filter(|b| b != new)
            .or_else(|| self.git(&["merge-base", new, &remote_base]))
    }

    /// Where a review, fix or CI checkout takes its steer files (`is_steer`): the trunk's merge-base with `head`, else
    /// the remote trunk's tip. Never the diff base, which for a follow-up push is the PR's own previous commit.
    /// None when neither resolves: there is nothing trusted to take them from.
    pub fn steer_base(&self, head: &str, base: &str) -> Option<String> {
        self.range(None, head, base)
            .or_else(|| self.git(&["rev-parse", "--verify", "--quiet", &format!("{}/{base}^{{commit}}", self.remote)]))
    }

    pub fn numstat(&self, base: &str, head: &str) -> Vec<(u32, u32, String)> {
        self.git(&["diff", "--numstat", base, head]).unwrap_or_default().lines()
            .filter_map(|l| {
                let mut p = l.splitn(3, '\t');
                let a = p.next()?.parse().unwrap_or(0);
                let d = p.next()?.parse().unwrap_or(0);
                Some((a, d, p.next()?.to_string()))
            })
            .collect()
    }

    /// Paths changed in base..head, renames by their new path (numstat shows renames as `old => new`).
    /// None when git fails: an empty set reads as "nothing changed" and would demote every on-base CI finding.
    /// The range's changed files that exist at `head`, as plain paths: deletes dropped, renames as their new
    /// path (numstat's `dir/{a => b}` form matches nothing a reviewer lists). What a review can open.
    pub fn files_at_head(&self, base: &str, head: &str) -> Vec<String> {
        self.git(&["diff", "--name-only", "--no-renames", "--diff-filter=d", base, head])
            .unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// A detached checkout of `head` for one review: `/code-review` reads the working tree, and the main
    /// checkout is on whatever branch someone left it. A leftover from a crashed run is replaced.
    /// The checkout's Claude files (`.claude/**`, every `CLAUDE.md`, `REVIEW.md`, `.review/`) come from `steer`
    /// (`steer_base`: the trunk, never the PR): they instruct the reviewer, so a PR could otherwise widen what the
    /// engine may run (a committed allow-list) or tell it what to report. The PR's own edits to them are still in the
    /// reviewed diff.
    pub fn add_worktree(&self, path: &Path, steer: &str, head: &str) -> bool {
        self.remove_worktree(path);
        if !self.ok(&["worktree", "add", "--detach", "--force", &path.to_string_lossy(), head]) { return false; }
        // -z: NUL-separated and unquoted, so non-ASCII paths match the filter as written.
        let files = |rev: &str| -> Vec<String> {
            self.git(&["ls-tree", "-r", "-z", "--name-only", rev]).unwrap_or_default().split('\0')
                .filter(|p| !p.is_empty() && is_steer(p)).map(str::to_string).collect()
        };
        for p in files(head) { let _ = std::fs::remove_file(path.join(&p)); }
        let _ = std::fs::remove_dir_all(path.join(".claude"));
        let _ = std::fs::remove_dir_all(path.join(".review"));
        let ok = self.checkout_from(path, steer, &files(steer));
        if !ok { self.remove_worktree(path); } // never leave a half-populated checkout behind
        ok
    }

    /// Every base guidance file must land: a review without the base's instructions is not a trusted review, so a
    /// miss fails the checkout (and `Queue::retry` gives it three attempts).
    pub fn checkout_from(&self, path: &Path, base: &str, files: &[String]) -> bool {
        files.iter().all(|p| Command::new("git").arg("-C").arg(path).args(["checkout", base, "--", p])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().map(|s| s.success()).unwrap_or(false))
    }

    pub fn remove_worktree(&self, path: &Path) {
        if !path.exists() { let _ = self.ok(&["worktree", "prune"]); return; }
        let _ = self.ok(&["worktree", "remove", "--force", &path.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(path);
        let _ = self.ok(&["worktree", "prune"]);
    }

    pub fn changed_files(&self, base: &str, head: &str) -> Option<std::collections::HashSet<String>> {
        Some(self.git(&["diff", "--name-only", base, head])?.lines().map(str::to_string).collect())
    }

    pub fn messages(&self, base: &str, head: &str) -> String {
        self.git(&["log", "--format=%s%n%b", &format!("{base}..{head}")]).unwrap_or_default()
    }

    pub fn added_lines(&self, base_ref: &str, head: &str) -> HashMap<String, HashSet<u32>> {
        parse_added_lines(&self.git(&["diff", "-U0", &format!("{base_ref}...{head}")]).unwrap_or_default())
    }
}

pub fn parse_added_lines(diff: &str) -> HashMap<String, HashSet<u32>> {
    let mut out: HashMap<String, HashSet<u32>> = HashMap::new();
    let mut file: Option<String> = None;
    for l in diff.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            file = p.strip_prefix("b/").map(str::to_string);
        } else if let (Some(f), Some(h)) = (&file, l.strip_prefix("@@ ")) {
            // "@@ -a[,b] +c[,d] @@"
            if let Some(plus) = h.split(' ').find(|t| t.starts_with('+')) {
                let mut it = plus[1..].splitn(2, ',');
                let c: u32 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let d: u32 = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
                out.entry(f.clone()).or_default().extend(c..c + d);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output().unwrap();
        assert!(o.status.success(), "{:?}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn commit(dir: &Path, file: &str, body: &str) -> String {
        std::fs::write(dir.join(file), body).unwrap();
        sh(dir, &["add", "."]);
        sh(dir, &["commit", "-qm", file]);
        sh(dir, &["rev-parse", "HEAD"])
    }
    fn fixture() -> (tempfile::TempDir, Repo, String) {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "uat"]);
        sh(d, &["remote", "add", "origiin", "/nonexistent"]);
        let base = commit(d, "a.txt", "1\n");
        sh(d, &["update-ref", "refs/remotes/origiin/uat", &base]);
        let repo = Repo::discover(d).unwrap();
        (t, repo, base)
    }

    #[test]
    fn a_branch_maps_to_the_worktree_it_is_checked_out_in() {
        let (t, repo, _) = fixture();
        let wt = tempfile::tempdir().unwrap();
        let d = wt.path().join("agent-a");
        sh(t.path(), &["worktree", "add", "-q", "-b", "feat/x", d.to_str().unwrap()]);
        assert_eq!(repo.worktree_of("uat"), Some(repo.root.clone()));
        assert_eq!(repo.worktree_of("feat/x"), Some(d.canonicalize().unwrap()));
        assert_eq!(repo.worktree_of("feat"), None, "a prefix of a branch is not that branch");
        assert_eq!(repo.worktree_of("nope"), None);
    }
    #[test]
    fn files_at_head_drop_deletes_and_split_renames() {
        let (t, repo, base) = fixture();
        let d = t.path();
        commit(d, "b.txt", "a long enough body for rename detection to pair it\n");
        let from = sh(d, &["rev-parse", "HEAD"]);
        sh(d, &["rm", "-q", "a.txt"]);
        sh(d, &["mv", "b.txt", "c.txt"]);
        let head = commit(d, "e.txt", "new\n");
        let mut got = repo.files_at_head(&from, &head);
        got.sort();
        // a.txt deleted, b.txt renamed to c.txt (reported as c.txt, never "{b.txt => c.txt}"), e.txt added.
        assert_eq!(got, vec!["c.txt", "e.txt"]);
        assert!(repo.files_at_head(&base, &base).is_empty());
    }
    #[test]
    fn changed_in_keeps_non_ascii_paths_and_renames_and_drops_steer_files() {
        let (t, repo, _) = fixture();
        commit(t.path(), "old.txt", "a body long enough for rename detection to pair it up\n");
        let wt = tempfile::tempdir().unwrap();
        let d = &wt.path().join("fix");
        sh(t.path(), &["worktree", "add", "-q", "--detach", d.to_str().unwrap(), "HEAD"]);
        std::fs::write(d.join("a.txt"), "2\n").unwrap(); // modified
        std::fs::write(d.join("é.ts"), "x\ny\n").unwrap(); // new, untracked, non-ASCII
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/settings.json"), "{}\n").unwrap(); // steer: never the fix's work
        sh(d, &["mv", "old.txt", "new.txt"]);
        std::fs::write(d.join("stray.ts"), "z\n").unwrap(); // new but not named: never added, so never seen
        // `old.txt` no longer exists: it is not handed to `add`, which would fail on a pathspec matching nothing.
        let files: Vec<String> = ["a.txt", "é.ts", "new.txt", "old.txt", ".claude/settings.json"].map(String::from).into();
        let mut got = repo.changed_in(d, &files);
        got.sort_by(|a, b| a.2.cmp(&b.2));
        let want: Vec<(u32, u32, String)> = vec![(1, 1, "a.txt".into()), (0, 0, "new.txt".into()),
                                                 (0, 0, "old.txt".into()), (2, 0, "é.ts".into())];
        assert_eq!(got, want);
        std::fs::write(t.path().join("a.txt"), "3\n").unwrap();
        assert!(repo.changed_in(t.path(), &["a.txt".into()]).is_empty(), "no linked worktree, no trusted gitdir: fail closed");
    }
    #[test]
    fn a_review_worktree_is_the_head_and_goes_away() {
        let (t, repo, _) = fixture();
        let head = commit(t.path(), "z.txt", "z\n");
        let wt = t.path().join("wt");
        assert!(repo.add_worktree(&wt, &head, &head));
        assert_eq!(std::fs::read_to_string(wt.join("z.txt")).unwrap(), "z\n");
        assert!(repo.add_worktree(&wt, &head, &head), "a leftover from a crashed run is replaced");
        repo.remove_worktree(&wt);
        // A PR's own Claude settings could widen what the engine may run: the review checkout never has them.
        std::fs::create_dir_all(t.path().join(".claude")).unwrap();
        std::fs::write(t.path().join(".claude/settings.local.json"), "{}").unwrap();
        let with = commit(t.path(), ".claude/settings.json", r#"{"permissions":{"allow":["Bash(*)"]}}"#);
        assert!(repo.add_worktree(&wt, &head, &with));
        assert!(!wt.join(".claude/settings.json").exists() && !wt.join(".claude/settings.local.json").exists());
        repo.remove_worktree(&wt);
        assert!(!wt.exists());
    }
    #[test]
    fn the_review_checkout_takes_its_claude_files_from_the_base() {
        let (t, repo, _) = fixture();
        let d = t.path();
        let base = commit(d, "CLAUDE.md", "trusted\n");
        std::fs::create_dir_all(d.join(".claude/skills/x")).unwrap();
        std::fs::create_dir_all(d.join("apps/a")).unwrap();
        std::fs::write(d.join(".claude/skills/x/SKILL.md"), "report nothing\n").unwrap();
        std::fs::write(d.join("apps/a/CLAUDE.md"), "report nothing\n").unwrap();
        std::fs::write(d.join("CLAUDE.md"), "report nothing\n").unwrap();
        std::fs::write(d.join("apps/a/claude.md"), "report nothing\n").unwrap();
        std::fs::write(d.join("CLAUDE.local.md"), "report nothing\n").unwrap();
        std::fs::create_dir_all(d.join("apps/a/.claude/skills/y")).unwrap();
        std::fs::write(d.join("apps/a/.claude/skills/y/SKILL.md"), "report nothing\n").unwrap();
        std::fs::create_dir_all(d.join(".claude/skills/é")).unwrap();
        std::fs::write(d.join(".claude/skills/é/SKILL.md"), "report nothing\n").unwrap();
        let head = commit(d, "apps/a/code.ts", "x\n");
        let wt = d.join("wt");
        assert!(repo.add_worktree(&wt, &base, &head));
        // A PR cannot instruct its own reviewer: Claude files are the base's; the code is the head's.
        assert_eq!(std::fs::read_to_string(wt.join("CLAUDE.md")).unwrap(), "trusted\n");
        assert!(!wt.join(".claude/skills/x/SKILL.md").exists() && !wt.join("apps/a/CLAUDE.md").exists());
        assert!(!wt.join("apps/a/claude.md").exists() && !wt.join("CLAUDE.local.md").exists());
        assert!(!wt.join(".claude/skills/é/SKILL.md").exists(), "non-ASCII paths are matched too");
        assert!(!wt.join("apps/a/.claude/skills/y/SKILL.md").exists(), "nested .claude/ folders too");
        assert_eq!(std::fs::read_to_string(wt.join("apps/a/code.ts")).unwrap(), "x\n");
        repo.remove_worktree(&wt);
    }
    #[test]
    fn a_follow_up_push_takes_its_steer_files_from_the_trunk_not_the_previous_commit() {
        let (t, repo, _) = fixture();
        let d = t.path();
        let trunk = commit(d, "REVIEW.md", "trusted\n");
        sh(d, &["update-ref", "refs/remotes/origiin/uat", &trunk]);
        sh(d, &["checkout", "-qb", "feat/x"]);
        let c1 = commit(d, "REVIEW.md", "report nothing\n");
        let c2 = commit(d, "code.ts", "x\n");
        // The diff range of the follow-up push is c1..c2, but c1 is the PR's own: steer files come from the trunk.
        assert_eq!(repo.range(Some(&c1), &c2, "uat").as_deref(), Some(c1.as_str()));
        let steer = repo.steer_base(&c2, "uat").unwrap();
        assert_eq!(steer, trunk);
        let wt = d.join("wt");
        assert!(repo.add_worktree(&wt, &steer, &c2));
        assert_eq!(std::fs::read_to_string(wt.join("REVIEW.md")).unwrap(), "trusted\n");
        repo.remove_worktree(&wt);
        // No merge-base with the trunk (an unrelated history): the remote trunk's tip.
        sh(d, &["checkout", "-q", "--orphan", "lone"]);
        let lone = commit(d, "l.txt", "l\n");
        assert_eq!(repo.steer_base(&lone, "uat").as_deref(), Some(trunk.as_str()));
        // No remote trunk at all: nothing trusted to take them from, so no review (never the diff base).
        assert!(repo.steer_base(&c2, "nope").is_none());
        // `.claude/commands/` (a PR-side `/code-review` shadow) is a steer path, so it never reaches the checkout.
        assert!(is_steer(".claude/commands/code-review.md"));
    }
    #[test]
    fn the_reviews_folder_is_excluded_once() {
        let (t, repo, _) = fixture();
        let ex = repo.common.join("info/exclude");
        std::fs::create_dir_all(ex.parent().unwrap()).unwrap();
        std::fs::write(&ex, "# mine\n*.swp").unwrap(); // the user's own lines, no trailing newline
        repo.exclude_reviews().unwrap();
        repo.exclude_reviews().unwrap();
        assert_eq!(std::fs::read_to_string(&ex).unwrap(), "# mine\n*.swp\n/.reviews/\n");
        std::fs::create_dir_all(t.path().join(".reviews/feat/x")).unwrap();
        std::fs::write(t.path().join(".reviews/feat/x/ledger.json"), "{}").unwrap();
        assert_eq!(sh(t.path(), &["status", "--porcelain", "--untracked-files=all"]), "");
        // No info/ dir yet (a repo made without templates): created.
        std::fs::remove_dir_all(repo.common.join("info")).unwrap();
        repo.exclude_reviews().unwrap();
        assert_eq!(std::fs::read_to_string(&ex).unwrap(), "/.reviews/\n");
    }
    #[test]
    fn a_base_guidance_file_that_will_not_check_out_stops_the_review() {
        let (t, repo, base) = fixture();
        let wt = t.path().join("wt");
        assert!(repo.add_worktree(&wt, &base, &base));
        assert!(repo.checkout_from(&wt, &base, &["a.txt".into()]));
        // Reviewing without one of the base's guidance files must not pass as a trusted checkout.
        assert!(!repo.checkout_from(&wt, &base, &["a.txt".into(), "gone/CLAUDE.md".into()]));
        repo.remove_worktree(&wt);
    }
    #[test]
    fn discover_reads_remote_and_common_dir() {
        let (t, repo, _) = fixture();
        assert_eq!(repo.remote, "origiin");
        assert_eq!(repo.root, t.path().canonicalize().unwrap());
    }
    #[test]
    fn snapshot_keeps_slashes() {
        let (t, repo, base) = fixture();
        sh(t.path(), &["update-ref", "refs/remotes/origiin/feat/x", &base]);
        let s = repo.snapshot().unwrap();
        assert_eq!(s.get("feat/x"), Some(&base));
        assert_eq!(s.get("uat"), Some(&base));
    }
    #[test]
    fn failed_snapshot_is_none_not_empty() {
        let (t, repo, _) = fixture();
        let broken = Repo { root: t.path().join("gone"), common: repo.common.clone(), remote: repo.remote.clone() };
        assert!(broken.snapshot().is_none());
    }
    #[test]
    fn range_fast_forward_first_push_and_force_push() {
        let (t, repo, base) = fixture();
        let d = t.path();
        sh(d, &["checkout", "-qb", "feat/x"]);
        let c1 = commit(d, "b.txt", "b\n");
        let c2 = commit(d, "c.txt", "c\n");
        assert_eq!(repo.range(Some(&c1), &c2, "uat").as_deref(), Some(c1.as_str()));
        assert_eq!(repo.range(None, &c2, "uat").as_deref(), Some(base.as_str()));
        sh(d, &["reset", "-q", "--hard", &base]);
        let c3 = commit(d, "d.txt", "d\n"); // c2 is no longer an ancestor
        assert_eq!(repo.range(Some(&c2), &c3, "uat").as_deref(), Some(base.as_str()));
    }
    #[test]
    fn first_push_ignores_unpushed_local_uat_commits() {
        let (t, repo, _) = fixture();
        let d = t.path();
        let local_uat = commit(d, "u.txt", "unpushed\n"); // local uat ahead of origiin/uat
        sh(d, &["checkout", "-qb", "feat/y"]);
        let head = commit(d, "y.txt", "y\n");
        assert_eq!(repo.range(None, &head, "uat").as_deref(), Some(local_uat.as_str()));
    }
    #[test]
    fn branch_cut_from_unpushed_local_uat_reviews_against_remote_uat() {
        let (t, repo, base) = fixture();
        let local_uat = commit(t.path(), "u.txt", "unpushed\n"); // the branch IS local uat's unpushed work
        assert_eq!(repo.range(None, &local_uat, "uat").as_deref(), Some(base.as_str()));
    }
    #[test]
    fn range_uses_the_configured_base() {
        let (t, repo, base) = fixture();
        let d = t.path();
        sh(d, &["checkout", "-qb", "develop"]);
        let dev = commit(d, "d.txt", "d\n");
        sh(d, &["update-ref", "refs/remotes/origiin/develop", &dev]);
        let head = commit(d, "h.txt", "h\n");
        assert_eq!(repo.range(None, &head, "develop").as_deref(), Some(dev.as_str()));
        assert_eq!(repo.range(None, &head, "uat").as_deref(), Some(base.as_str()));
    }
    #[test]
    fn the_review_config_comes_from_the_base_too() {
        let (t, repo, _) = fixture();
        let d = t.path();
        std::fs::create_dir_all(d.join(".review")).unwrap();
        let base = commit(d, ".review/config.toml", "[fix]\nenabled = false\n");
        let head = commit(d, ".review/config.toml", "[fix]\nenabled = true\n");
        let wt = d.join("wt");
        assert!(repo.add_worktree(&wt, &base, &head));
        assert_eq!(std::fs::read_to_string(wt.join(".review/config.toml")).unwrap(), "[fix]\nenabled = false\n");
        repo.remove_worktree(&wt);
        assert!(is_steer(".review/config.toml") && is_steer("a/CLAUDE.md") && !is_steer("src/review.rs"));
    }
    #[test]
    fn changed_files_names_renames_by_new_path() {
        let (t, repo, base) = fixture();
        sh(t.path(), &["mv", "a.txt", "renamed.txt"]);
        sh(t.path(), &["commit", "-qm", "mv"]);
        let head = sh(t.path(), &["rev-parse", "HEAD"]);
        let c = repo.changed_files(&base, &head).unwrap();
        assert!(c.contains("renamed.txt"), "{c:?}");
        assert!(!c.iter().any(|p| p.contains("=>")));
        let broken = Repo { root: t.path().join("gone"), common: repo.common.clone(), remote: repo.remote.clone() };
        assert!(broken.changed_files(&base, &head).is_none(), "a failed diff must not read as 'nothing changed'");
    }
    #[test]
    fn numstat_and_messages() {
        let (t, repo, base) = fixture();
        let head = commit(t.path(), "b.txt", "x\ny\n");
        assert_eq!(repo.numstat(&base, &head), vec![(2, 0, "b.txt".to_string())]);
        assert!(repo.messages(&base, &head).contains("b.txt"));
    }
    #[test]
    fn parses_added_lines() {
        let diff = "diff --git a/x.ts b/x.ts\n+++ b/x.ts\n@@ -1,0 +2,3 @@\n+a\n+b\n+c\n@@ -9 +12 @@\n-o\n+n\n+++ b/gone.ts\n@@ -1,2 +0,0 @@\n";
        let m = parse_added_lines(diff);
        let mut x: Vec<u32> = m["x.ts"].iter().copied().collect();
        x.sort();
        assert_eq!(x, vec![2, 3, 4, 12]);
        assert!(m.get("gone.ts").map_or(true, |s| s.is_empty()));
    }
}
