//! The PR inbox: `@review` mentions and 👍/👎 reactions, polled with `gh api`, with no webhook and no public URL.
//! `poll` turns them into queue actions; the daemon owns the queue and the branch snapshot and applies them.
//! @see README.md
use crate::config::Config;
use crate::github::{self, IssueComment, Pr, Reaction, ReviewComment};
use crate::ledger::Ledger;
use crate::mention::{self, Act, Cmd, Perms, State};
use crate::queue::Request;
use std::path::{Path, PathBuf};

macro_rules! log { ($($t:tt)*) => { eprintln!("[{}] {}", crate::daemon::now(), format!($($t)*)) } }

/// The GitHub calls the inbox makes; `Live` makes them with `gh`, the tests with a fake.
pub trait Gh {
    fn comments_since(&self, since: &str) -> Result<Vec<IssueComment>, String>;
    fn permission(&self, login: &str) -> Result<String, String>;
    fn open_prs(&self) -> Result<Vec<(String, Pr)>, String>;
    fn review_comments(&self, pr: u64) -> Result<Vec<ReviewComment>, String>;
    fn reactions(&self, comment_id: u64) -> Result<Vec<Reaction>, String>;
    fn say(&self, pr: u64, body: &str);
    fn sync(&self, pr: &Pr, ledger: &Ledger);
}

pub struct Live { pub slug: String }

impl Gh for Live {
    fn comments_since(&self, since: &str) -> Result<Vec<IssueComment>, String> { github::issue_comments_since(&self.slug, since) }
    fn permission(&self, login: &str) -> Result<String, String> { github::permission(&self.slug, login) }
    fn open_prs(&self) -> Result<Vec<(String, Pr)>, String> { github::open_prs() }
    fn review_comments(&self, pr: u64) -> Result<Vec<ReviewComment>, String> { github::review_comments(&self.slug, pr) }
    fn reactions(&self, id: u64) -> Result<Vec<Reaction>, String> { github::reactions(&self.slug, id) }
    fn say(&self, pr: u64, body: &str) {
        if let Err(e) = github::post_issue_comment(&self.slug, pr, body) { log!("comment on #{pr}: {e}"); }
    }
    fn sync(&self, pr: &Pr, l: &Ledger) {
        if let Err(e) = github::sync_pr(&self.slug, pr, &l.closed_comment_ids(), &l.open_comment_ids(), true) {
            log!("sync #{}: {e}", pr.number);
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    /// `pr`: where the daemon answers, with 👀 once queued or a reply when it cannot queue.
    Review { branch: String, pr: u64, model: Option<String>, request: Request },
    Fix { branch: String, pr: u64, ids: Vec<String>, login: String, comment_id: Option<u64> },
}

/// Failed permission lookups per id, so a lookup that keeps failing is dropped (and logged) rather than retried forever.
#[derive(Default)]
struct Retries(std::collections::HashMap<u64, u8>);

const LOOKUP_TRIES: u8 = 3;

impl Retries {
    /// Records one failed lookup for `id`: Some(attempts so far) while it should be tried again, None on the last.
    fn failed(&mut self, id: u64) -> Option<u8> {
        let n = self.0.entry(id).or_default();
        *n += 1;
        if *n < LOOKUP_TRIES { return Some(*n); }
        self.0.remove(&id);
        None
    }
    fn clear(&mut self, id: u64) { self.0.remove(&id); }
}

pub struct Inbox {
    pub state: State,
    perms: Perms,
    /// Comments (by comment id) and reactions (by reaction id) whose permission lookup errored, and how often.
    comment_retries: Retries,
    reaction_retries: Retries,
    /// Seconds between polls: `triggers.poll_secs`, doubled while GitHub rate-limits.
    pub every: u64,
    last: u64,
    path: PathBuf,
    last_err: Option<String>,
}

impl Inbox {
    /// The first run starts from `now`: comments written before revaiw was running are history, not requests.
    pub fn load(dir: &Path, cfg: &Config, now: u64) -> Inbox {
        let path = dir.join("mentions.json");
        let mut state = State::load(&path);
        if state.since.is_empty() { state.since = mention::iso(now); }
        Inbox { state, perms: Perms::default(), comment_retries: Retries::default(), reaction_retries: Retries::default(), every: cfg.triggers.poll_secs.max(10), last: 0, path, last_err: None }
    }

    pub fn due(&self, now: u64) -> bool { now.saturating_sub(self.last) >= self.every }

    pub fn poll(&mut self, gh: &dyn Gh, cfg: &Config, now: u64, ledger_of: &dyn Fn(&str) -> PathBuf) -> Vec<Action> {
        self.last = now;
        let mut acts = vec![];
        let prs = match gh.open_prs() {
            Ok(p) => p,
            Err(e) => { self.failed(cfg, e); return acts; }
        };
        match gh.comments_since(&self.state.since) {
            Ok(comments) => {
                self.every = mention::next_interval(self.every, cfg.triggers.poll_secs.max(10), None);
                self.last_err = None;
                // A comment held for a lookup retry must stay inside the next poll's `since` window.
                let since = self.state.since.clone();
                let mut held = false;
                for c in comments { held |= self.mention(gh, cfg, now, &prs, c, &mut acts); }
                if held { self.state.since = since; }
            }
            Err(e) => { self.failed(cfg, e); return acts; }
        }
        for (branch, pr) in &prs { self.reactions(gh, cfg, now, branch, pr, &ledger_of(branch), &mut acts); }
        self.state.save(&self.path);
        acts
    }

    fn failed(&mut self, cfg: &Config, e: String) {
        self.every = mention::next_interval(self.every, cfg.triggers.poll_secs.max(10), Some(&e));
        if self.last_err.as_deref() != Some(e.as_str()) { log!("inbox: {e} (next poll in {}s)", self.every); }
        self.last_err = Some(e);
    }

    /// True when the comment is held: its permission lookup errored, so it is not remembered and the next poll
    /// retries it (up to `LOOKUP_TRIES` times). A denial is an answer and is remembered at once.
    fn mention(&mut self, gh: &dyn Gh, cfg: &Config, now: u64, prs: &[(String, Pr)], c: IssueComment, acts: &mut Vec<Action>) -> bool {
        if self.state.seen.contains(&c.id) { return false; }
        // `since` filters by update time, so an old comment edited later comes back: it never runs (spec §5.2.5),
        // and is not remembered, so it cannot evict a newer id from `seen`.
        if c.created_at.as_str() < self.state.since.as_str() { return false; }
        let Some(cmd) = mention::parse(&c.body, &cfg.triggers.mention) else {
            self.state.remember(c.id, &c.created_at);
            return false;
        };
        match self.perms.allowed(&c.login, now, &|l| gh.permission(l)) {
            Ok(true) => self.comment_retries.clear(c.id),
            Ok(false) => {
                self.comment_retries.clear(c.id);
                self.state.remember(c.id, &c.created_at);
                log!("mention by {} on #{} ignored: no write permission", c.login, c.pr);
                return false;
            }
            Err(e) => {
                let held = match self.comment_retries.failed(c.id) {
                    Some(n) => { log!("mention by {} on #{}: permission lookup failed: {e}; retrying (attempt {n} of {LOOKUP_TRIES})", c.login, c.pr); true }
                    None => { log!("mention by {} on #{}: permission lookup failed {LOOKUP_TRIES} times: {e}; dropped", c.login, c.pr); false }
                };
                if !held { self.state.remember(c.id, &c.created_at); }
                return held;
            }
        }
        self.state.remember(c.id, &c.created_at);
        let Some((branch, _)) = prs.iter().find(|(_, p)| p.number == c.pr) else {
            log!("mention on #{} ignored: not an open PR", c.pr);
            return false;
        };
        match cmd {
            Cmd::Review { model, text } => acts.push(Action::Review { branch: branch.clone(), pr: c.pr, model,
                request: Request { login: c.login.clone(), text, comment_id: c.id } }),
            Cmd::Fix { .. } if cfg.fix.enabled && cfg.is_trunk(branch) => {
                // A promotion PR's head is the trunk: a fix would land there with no review.
                gh.say(c.pr, &format!("{}\nrevaiw never pushes a fix to `{branch}` because it is a trunk branch. \
                                       Fix it on a feature branch instead.", crate::render::MARKER));
            }
            Cmd::Fix { ids } if cfg.fix.enabled => acts.push(Action::Fix { branch: branch.clone(), pr: c.pr, ids,
                login: c.login.clone(), comment_id: Some(c.id) }),
            Cmd::Fix { .. } => {
                gh.say(c.pr, &format!("{}\nFixes are off for this repo. Set `[fix] enabled = true` in \
                                       `.review/config.toml` on the default branch to let revaiw push fixes.",
                                      crate::render::MARKER));
            }
        }
        false
    }

    /// 👍/❤️ on a revaiw finding queues its fix; 👎/😕 dismisses it. Only comments whose relevant-reaction count
    /// rose since the last poll are fetched, and only the reactions past the stored count are new.
    fn reactions(&mut self, gh: &dyn Gh, cfg: &Config, now: u64, branch: &str, pr: &Pr, lp: &Path, acts: &mut Vec<Action>) {
        let mut ledger = Ledger::load(lp);
        let ours: std::collections::HashMap<u64, String> = ledger.findings.iter()
            .filter_map(|(id, e)| e.comment_id.map(|c| (c, id.clone()))).collect();
        if ours.is_empty() { return; }
        let Ok(comments) = gh.review_comments(pr.number) else { return };
        let mut dismissed = false;
        for rc in comments {
            let Some(fid) = ours.get(&rc.id) else { continue };
            let prev = self.state.reactions.get(&rc.id).copied().unwrap_or(0);
            self.state.reactions.insert(rc.id, rc.relevant);
            if rc.relevant <= prev { continue; }
            let Ok(list) = gh.reactions(rc.id) else { self.state.reactions.insert(rc.id, prev); continue };
            for (n, r) in list.into_iter().filter(|r| mention::act(&r.content).is_some()).enumerate().skip(prev as usize) {
                match self.perms.allowed(&r.login, now, &|l| gh.permission(l)) {
                    Ok(true) => self.reaction_retries.clear(r.id),
                    Ok(false) => { self.reaction_retries.clear(r.id); continue; }
                    Err(e) => match self.reaction_retries.failed(r.id) {
                        // Count only what was handled before it, so the next poll fetches this reaction again.
                        Some(t) => {
                            log!("reaction by {} on comment {}: permission lookup failed: {e}; retrying (attempt {t} of {LOOKUP_TRIES})", r.login, rc.id);
                            self.state.reactions.insert(rc.id, n as u32);
                            break;
                        }
                        None => { log!("reaction by {} on comment {}: permission lookup failed {LOOKUP_TRIES} times: {e}; dropped", r.login, rc.id); continue; }
                    },
                }
                match mention::act(&r.content) {
                    Some(Act::Fix) if cfg.fix.enabled && cfg.is_trunk(branch) => {
                        log!("reaction by {} on #{}: no fix queued, `{branch}` is a trunk branch", r.login, pr.number);
                    }
                    Some(Act::Fix) if cfg.fix.enabled => acts.push(Action::Fix { branch: branch.into(), pr: pr.number, ids: vec![fid.clone()],
                                                                                login: r.login.clone(), comment_id: None }),
                    Some(Act::Dismiss) => dismissed |= ledger.dismiss(fid, &format!("👎 by {}", r.login)),
                    _ => {}
                }
            }
        }
        if dismissed { ledger.save(lp); gh.sync(pr, &ledger); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{resolve, Config};
    use crate::github::{IssueComment, Pr, Reaction, ReviewComment};
    use crate::model::{Finding, ReviewOutput};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        comments: Vec<IssueComment>,
        perms: Vec<(&'static str, &'static str)>,
        prs: Vec<(String, u64)>,
        review_comments: Vec<ReviewComment>,
        reactions: Vec<Reaction>,
        fail: Option<String>,
        /// The next this-many permission lookups fail (a transient `gh` error), then they answer from `perms`.
        perm_failures: std::cell::Cell<u32>,
        log: RefCell<Vec<String>>,
    }
    impl Gh for Fake {
        fn comments_since(&self, since: &str) -> Result<Vec<IssueComment>, String> {
            self.log.borrow_mut().push(format!("since {since}"));
            match &self.fail { Some(e) => Err(e.clone()), None => Ok(self.comments.clone()) }
        }
        fn permission(&self, login: &str) -> Result<String, String> {
            if self.perm_failures.get() > 0 { self.perm_failures.set(self.perm_failures.get() - 1); return Err("HTTP 502".into()); }
            self.perms.iter().find(|(l, _)| *l == login).map(|(_, p)| p.to_string()).ok_or("HTTP 404".into())
        }
        fn open_prs(&self) -> Result<Vec<(String, Pr)>, String> {
            Ok(self.prs.iter().map(|(b, n)| (b.clone(), Pr { number: *n, base: "main".into() })).collect())
        }
        fn review_comments(&self, _pr: u64) -> Result<Vec<ReviewComment>, String> { Ok(self.review_comments.clone()) }
        fn reactions(&self, _id: u64) -> Result<Vec<Reaction>, String> { Ok(self.reactions.clone()) }
        fn say(&self, pr: u64, body: &str) { self.log.borrow_mut().push(format!("say {pr} {body}")); }
        fn sync(&self, _pr: &Pr, _l: &Ledger) { self.log.borrow_mut().push("sync".into()); }
    }
    fn cfg(fix: bool) -> Config { let mut c = resolve(Config::default(), "main"); c.fix.enabled = fix; c }
    fn comment(id: u64, login: &str, body: &str) -> IssueComment {
        IssueComment { id, pr: 7, login: login.into(), body: body.into(), created_at: format!("2026-10-15T10:00:{id:02}Z") }
    }
    fn inbox(d: &std::path::Path) -> Inbox {
        let mut i = Inbox::load(d, &cfg(true), 0);
        i.state.since = "2026-10-15T00:00:00Z".into();
        i
    }

    #[test] fn a_writers_mention_becomes_a_review_with_its_request_and_gets_eyes() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review is the retry safe?")], perms: vec![("kim", "write")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        let acts = inbox(d.path()).poll(&gh, &cfg(true), 100, &|b| d.path().join(b));
        assert_eq!(acts, vec![Action::Review { branch: "feat/x".into(), pr: 7, model: None,
            request: Request { login: "kim".into(), text: "is the retry safe?".into(), comment_id: 1 } }]);
    }
    #[test] fn a_non_writer_is_ignored_silently() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "eve", "@review fix")], perms: vec![("eve", "read")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        assert!(inbox(d.path()).poll(&gh, &cfg(true), 100, &|b| d.path().join(b)).is_empty());
        assert!(!gh.log.borrow().iter().any(|l| l.starts_with("say")));
    }
    #[test] fn a_comment_runs_once_across_polls_and_restarts() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review")], perms: vec![("kim", "admin")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        let mut i = inbox(d.path());
        assert_eq!(i.poll(&gh, &cfg(true), 100, &|b| d.path().join(b)).len(), 1);
        assert!(i.poll(&gh, &cfg(true), 200, &|b| d.path().join(b)).is_empty());
        let mut again = Inbox::load(d.path(), &cfg(true), 300);
        assert!(again.poll(&gh, &cfg(true), 300, &|b| d.path().join(b)).is_empty(), "state survives a restart");
    }
    #[test] fn an_edited_old_comment_never_runs() {
        let d = tempfile::tempdir().unwrap();
        let old = IssueComment { id: 1, pr: 7, login: "kim".into(), body: "@review fix".into(),
                                 created_at: "2026-10-15T09:00:00Z".into() };
        let gh = Fake { comments: vec![old], perms: vec![("kim", "write")], prs: vec![("feat/x".into(), 7)], ..Default::default() };
        let mut i = inbox(d.path());
        i.state.since = "2026-10-15T10:00:00Z".into();
        assert!(i.poll(&gh, &cfg(true), 100, &|b| d.path().join(b)).is_empty());
        assert!(!gh.log.borrow().iter().any(|l| l.starts_with("say")));
        assert!(!i.state.seen.contains(&1), "never remembered, so it cannot evict a newer id");
    }
    #[test] fn a_fix_request_when_fixes_are_off_is_answered_not_queued() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review fix")], perms: vec![("kim", "write")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        assert!(inbox(d.path()).poll(&gh, &cfg(false), 100, &|b| d.path().join(b)).is_empty());
        assert!(gh.log.borrow().iter().any(|l| l.starts_with("say 7") && l.contains("enabled = true")));
    }
    #[test] fn a_fix_request_on_a_trunk_head_is_answered_not_queued() {
        let d = tempfile::tempdir().unwrap();
        // A promotion PR: head `uat` into `master`. Even with fixes on, the fix would land on the trunk unreviewed.
        let mut c = cfg(true);
        c.ci.branches.push("uat".into());
        let gh = Fake { comments: vec![comment(1, "kim", "@review fix")], perms: vec![("kim", "write")],
                        prs: vec![("uat".into(), 7)], ..Default::default() };
        assert!(inbox(d.path()).poll(&gh, &c, 100, &|b| d.path().join(b)).is_empty());
        assert!(gh.log.borrow().iter().any(|l| l.starts_with("say 7") && l.contains("`uat`") && l.contains("trunk")
                                                && l.contains("feature branch")));
    }
    #[test] fn fixes_off_is_answered_before_the_trunk_refusal() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review fix")], perms: vec![("kim", "write")],
                        prs: vec![("main".into(), 7)], ..Default::default() };
        assert!(inbox(d.path()).poll(&gh, &cfg(false), 100, &|b| d.path().join(b)).is_empty());
        assert!(gh.log.borrow().iter().any(|l| l.starts_with("say 7") && l.contains("enabled = true") && !l.contains("trunk")));
    }
    #[test] fn a_fix_request_on_a_feature_branch_is_still_queued() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review fix")], perms: vec![("kim", "write")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        assert_eq!(inbox(d.path()).poll(&gh, &cfg(true), 100, &|b| d.path().join(b)),
                   vec![Action::Fix { branch: "feat/x".into(), pr: 7, ids: vec![], login: "kim".into(), comment_id: Some(1) }]);
    }
    #[test] fn the_first_poll_starts_from_now_not_from_history() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake::default();
        let mut i = Inbox::load(d.path(), &cfg(true), 1_792_065_600);
        i.poll(&gh, &cfg(true), 1_792_065_600, &|b| d.path().join(b));
        assert!(gh.log.borrow().contains(&"since 2026-10-15T12:00:00Z".to_string()));
    }
    #[test] fn a_rate_limit_backs_off() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { fail: Some("HTTP 403: API rate limit exceeded".into()), ..Default::default() };
        let mut i = inbox(d.path());
        i.poll(&gh, &cfg(true), 100, &|b| d.path().join(b));
        assert_eq!(i.every, 120);
        assert!(!i.due(150) && i.due(220));
    }
    #[test] fn a_mention_whose_permission_lookup_errors_is_retried_on_the_next_poll() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review"), comment(2, "kim", "thanks")], perms: vec![("kim", "write")],
                        prs: vec![("feat/x".into(), 7)], perm_failures: 1.into(), ..Default::default() };
        let mut i = inbox(d.path());
        let since = i.state.since.clone();
        assert!(i.poll(&gh, &cfg(true), 100, &|b| d.path().join(b)).is_empty(), "the lookup failed: not run yet");
        assert!(!i.state.seen.contains(&1), "not remembered, so the next poll sees it again");
        assert_eq!(i.state.since, since, "the cursor does not pass a comment that still has to run");
        assert_eq!(i.poll(&gh, &cfg(true), 200, &|b| d.path().join(b)).len(), 1, "the retry runs it");
        assert!(i.state.seen.contains(&1));
        assert!(i.poll(&gh, &cfg(true), 300, &|b| d.path().join(b)).is_empty(), "and only once");
    }
    #[test] fn a_mention_is_dropped_after_three_failed_lookups() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "kim", "@review")], perms: vec![("kim", "write")],
                        prs: vec![("feat/x".into(), 7)], perm_failures: 99.into(), ..Default::default() };
        let mut i = inbox(d.path());
        for t in [100, 200] {
            assert!(i.poll(&gh, &cfg(true), t, &|b| d.path().join(b)).is_empty());
            assert!(!i.state.seen.contains(&1), "attempt at {t}: still retrying");
        }
        assert!(i.poll(&gh, &cfg(true), 300, &|b| d.path().join(b)).is_empty());
        assert!(i.state.seen.contains(&1), "the third failure remembers it");
        gh.perm_failures.set(0);
        assert!(i.poll(&gh, &cfg(true), 400, &|b| d.path().join(b)).is_empty(), "given up: it never runs");
    }
    #[test] fn a_denied_mention_is_remembered_at_once() {
        let d = tempfile::tempdir().unwrap();
        let gh = Fake { comments: vec![comment(1, "eve", "@review")], perms: vec![("eve", "read")],
                        prs: vec![("feat/x".into(), 7)], ..Default::default() };
        let mut i = inbox(d.path());
        i.poll(&gh, &cfg(true), 100, &|b| d.path().join(b));
        assert!(i.state.seen.contains(&1), "a denial is an answer, not an error");
    }
    #[test] fn a_reaction_whose_permission_lookup_errors_is_retried_without_losing_the_count() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("feat/x").join("ledger.json");
        let id = ledger_with_comment(&lp);
        let gh = Fake { perms: vec![("kim", "write")], prs: vec![("feat/x".into(), 7)],
                        review_comments: vec![ReviewComment { id: 55, relevant: 1 }], perm_failures: 1.into(),
                        reactions: vec![Reaction { id: 1, login: "kim".into(), content: "+1".into() }], ..Default::default() };
        let ledger_of = |b: &str| d.path().join(b).join("ledger.json");
        let mut i = inbox(d.path());
        assert!(i.poll(&gh, &cfg(true), 100, &ledger_of).is_empty());
        assert_eq!(i.state.reactions.get(&55).copied().unwrap_or(0), 0, "the count did not advance past the failed reaction");
        assert_eq!(i.poll(&gh, &cfg(true), 200, &ledger_of),
                   vec![Action::Fix { branch: "feat/x".into(), pr: 7, ids: vec![id], login: "kim".into(), comment_id: None }]);
        assert_eq!(i.state.reactions[&55], 1);
        assert!(i.poll(&gh, &cfg(true), 300, &ledger_of).is_empty(), "counted once it succeeded");
    }
    #[test] fn a_reaction_is_dropped_after_three_failed_lookups() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("feat/x").join("ledger.json");
        ledger_with_comment(&lp);
        let gh = Fake { perms: vec![("kim", "write")], prs: vec![("feat/x".into(), 7)],
                        review_comments: vec![ReviewComment { id: 55, relevant: 1 }], perm_failures: 99.into(),
                        reactions: vec![Reaction { id: 1, login: "kim".into(), content: "+1".into() }], ..Default::default() };
        let ledger_of = |b: &str| d.path().join(b).join("ledger.json");
        let mut i = inbox(d.path());
        for t in [100, 200] { i.poll(&gh, &cfg(true), t, &ledger_of); assert_eq!(i.state.reactions[&55], 0, "retrying at {t}"); }
        i.poll(&gh, &cfg(true), 300, &ledger_of);
        assert_eq!(i.state.reactions[&55], 1, "the third failure counts it");
        gh.perm_failures.set(0);
        assert!(i.poll(&gh, &cfg(true), 400, &ledger_of).is_empty());
    }
    fn ledger_with_comment(path: &std::path::Path) -> String {
        let mut l = Ledger::default();
        let f = Finding { severity: "important".into(), file: "a.ts".into(), line: 1, end_line: None, summary: "x".into(),
                          why: "y".into(), snippet: String::new(), lang: String::new(), suggested_patch: None,
                          suggestion: None, on_base: false };
        l.merge(&ReviewOutput { findings: vec![f], ..Default::default() }, "s");
        let id = l.findings.keys().next().unwrap().clone();
        l.findings.get_mut(&id).unwrap().comment_id = Some(55);
        l.save(path);
        id
    }
    #[test] fn a_thumbs_up_on_a_finding_queues_its_fix_and_a_thumbs_down_dismisses() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("feat/x").join("ledger.json");
        let id = ledger_with_comment(&lp);
        let mut gh = Fake { perms: vec![("kim", "write")], prs: vec![("feat/x".into(), 7)],
                            review_comments: vec![ReviewComment { id: 55, relevant: 1 }],
                            reactions: vec![Reaction { id: 1, login: "kim".into(), content: "+1".into() }], ..Default::default() };
        let ledger_of = |b: &str| d.path().join(b).join("ledger.json");
        let mut i = inbox(d.path());
        assert_eq!(i.poll(&gh, &cfg(true), 100, &ledger_of),
                   vec![Action::Fix { branch: "feat/x".into(), pr: 7, ids: vec![id.clone()], login: "kim".into(), comment_id: None }]);
        assert!(i.poll(&gh, &cfg(true), 200, &ledger_of).is_empty(), "a reaction counts once");
        gh.review_comments = vec![ReviewComment { id: 55, relevant: 2 }];
        gh.reactions.push(Reaction { id: 2, login: "kim".into(), content: "-1".into() });
        assert!(i.poll(&gh, &cfg(true), 300, &ledger_of).is_empty());
        assert_eq!(Ledger::load(&lp).findings[&id].status, "dismissed");
        assert!(gh.log.borrow().contains(&"sync".to_string()));
    }
    #[test] fn a_non_writers_reaction_does_nothing() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("feat/x").join("ledger.json");
        let id = ledger_with_comment(&lp);
        let mut gh = Fake { perms: vec![("eve", "read")], prs: vec![("feat/x".into(), 7)],
                            review_comments: vec![ReviewComment { id: 55, relevant: 1 }],
                            reactions: vec![Reaction { id: 1, login: "eve".into(), content: "+1".into() }], ..Default::default() };
        let ledger_of = |b: &str| d.path().join(b).join("ledger.json");
        let mut i = inbox(d.path());
        assert!(i.poll(&gh, &cfg(true), 100, &ledger_of).is_empty(), "a non-writer's 👍 queues no fix");
        gh.review_comments = vec![ReviewComment { id: 55, relevant: 2 }];
        gh.reactions.push(Reaction { id: 2, login: "eve".into(), content: "-1".into() });
        assert!(i.poll(&gh, &cfg(true), 200, &ledger_of).is_empty());
        assert_eq!(Ledger::load(&lp).findings[&id].status, "open", "a non-writer's 👎 dismisses nothing");
        assert!(!gh.log.borrow().contains(&"sync".to_string()));
    }
    #[test] fn a_thumbs_up_on_a_trunk_head_queues_no_fix_but_a_thumbs_down_still_dismisses() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("main").join("ledger.json");
        let id = ledger_with_comment(&lp);
        let mut gh = Fake { perms: vec![("kim", "write")], prs: vec![("main".into(), 7)],
                            review_comments: vec![ReviewComment { id: 55, relevant: 1 }],
                            reactions: vec![Reaction { id: 1, login: "kim".into(), content: "+1".into() }], ..Default::default() };
        let ledger_of = |b: &str| d.path().join(b).join("ledger.json");
        let mut i = inbox(d.path());
        assert!(i.poll(&gh, &cfg(true), 100, &ledger_of).is_empty(), "a 👍 on a trunk head queues no fix");
        gh.review_comments = vec![ReviewComment { id: 55, relevant: 2 }];
        gh.reactions.push(Reaction { id: 2, login: "kim".into(), content: "-1".into() });
        assert!(i.poll(&gh, &cfg(true), 200, &ledger_of).is_empty());
        assert_eq!(Ledger::load(&lp).findings[&id].status, "dismissed", "dismissing still works there");
    }
}
