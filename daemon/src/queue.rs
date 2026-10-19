use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Who asked for a review by `@review`, and what. Their text reaches the engine quoted, as their question.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Request {
    pub login: String,
    pub text: String,
    pub comment_id: u64,
}

/// A requested fix: these finding ids (empty = every open important one), asked for by `login`, by a
/// `@review fix` comment (`comment_id`) or a 👍 (None).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FixJob {
    pub ids: Vec<String>,
    pub login: String,
    #[serde(default)]
    pub comment_id: Option<u64>,
    pub queued_at: u64,
    #[serde(default)]
    pub attempts: u32,
    /// A retried fix waits until then (`retry_fix`), so its attempts are spread out, not spent in one tick.
    #[serde(default)]
    pub not_before: u64,
}

/// How long a fix that could not start or run waits before it is taken again.
const FIX_RETRY_AFTER: u64 = 90;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub base: String,
    pub head: String,
    pub queued_at: u64,
    /// `revaiw now --model <tier>`: route on that tier instead of the range's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Times this entry could not start (worktree checkout or spawn failed); see `Queue::retry`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    /// A `@review` mention's request. A later push keeps it: the question still applies to the newer head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Request>,
    /// `revaiw now` / `@review`: the whole branch, so a full review (nits listed again), never a follow-up.
    #[serde(default)]
    pub whole: bool,
}

fn is_zero(n: &u32) -> bool { *n == 0 }

#[derive(Debug, PartialEq)]
pub enum Retry { Queued, Superseded, GaveUp }

/// Ephemeral per-branch queue. Losing it costs a missed review, nothing more.
#[derive(Serialize, Deserialize, Default)]
pub struct Queue {
    pub entries: BTreeMap<String, Entry>,
    pub running: BTreeMap<String, Entry>,
    #[serde(default)]
    pub fixes: BTreeMap<String, FixJob>,
    /// Fixes taken and in flight, so a restart puts them back in `fixes` instead of requeueing their `running` slot as a review.
    #[serde(default)]
    pub running_fixes: BTreeMap<String, FixJob>,
}

impl Queue {
    /// `is_ancestor(a, b)`: a is an ancestor of b. A kept base must be one, or a force-push would review
    /// everything the rebase pulled in. While a review runs, the follow-up starts at its head, or at `base` when that
    /// is later (a merge `enqueue` narrowed to).
    pub fn push(&mut self, branch: &str, base: &str, head: &str, now: u64, is_ancestor: &dyn Fn(&str, &str) -> bool) {
        if let Some(e) = self.entries.get_mut(branch) {
            if !is_ancestor(&e.base, head) { e.base = base.into(); }
            e.head = head.into();
            e.queued_at = now;
            e.model = None;  // a pin belongs to its `revaiw now` request, not to later pushes
            e.attempts = 0;
            return;
        }
        let base = match self.running.get(branch) {
            Some(r) if is_ancestor(&r.head, head) => if is_ancestor(&r.head, base) { base.to_string() } else { r.head.clone() },
            _ => base.to_string(),
        };
        self.entries.insert(branch.into(), Entry { base, head: head.into(), queued_at: now, model: None, attempts: 0, request: None, whole: false });
    }

    /// A `revaiw now` request: always the whole branch (`base` is its merge-base with the base branch), replacing a
    /// pending entry's range and skipping push's running-head shortcut, which on an unchanged head would
    /// queue an empty range. Due at once.
    pub fn push_now(&mut self, branch: &str, base: &str, head: &str, model: Option<String>, request: Option<Request>) {
        self.entries.insert(branch.into(), Entry { base: base.into(), head: head.into(), queued_at: 0, model, attempts: 0, request, whole: true });
    }

    pub fn push_fix(&mut self, branch: &str, ids: Vec<String>, login: &str, comment_id: Option<u64>, now: u64) {
        match self.fixes.get_mut(branch) {
            Some(f) => {
                if f.ids.is_empty() || ids.is_empty() { f.ids.clear(); }
                else { for id in ids { if !f.ids.contains(&id) { f.ids.push(id); } } }
                f.login = login.into();
                if comment_id.is_some() { f.comment_id = comment_id; }
            }
            None => { self.fixes.insert(branch.into(), FixJob { ids, login: login.into(), comment_id, queued_at: now, attempts: 0, not_before: 0 }); }
        }
    }

    /// The next due fix (its branch has nothing running and any retry wait is over), at the branch's current remote
    /// head. It occupies the branch's `running` slot, so a review and a fix of one branch never run together. A
    /// branch that no longer exists drops its fix.
    pub fn take_fix(&mut self, now: u64, head_of: &dyn Fn(&str) -> Option<String>) -> Option<(String, FixJob, String)> {
        loop {
            let b = self.fixes.iter().find(|(b, f)| self.fix_ready(b, f, now))?.0.clone();
            let f = self.fixes.remove(&b)?;
            let Some(head) = head_of(&b) else { continue };
            self.running_fixes.insert(b.clone(), f.clone());
            self.running.insert(b.clone(), Entry { base: head.clone(), head: head.clone(), queued_at: f.queued_at,
                                                   model: None, attempts: 0, request: None, whole: false });
            return Some((b, f, head));
        }
    }

    /// Whether `take_fix` would take a fix now (before it asks for each branch's head).
    pub fn fix_due(&self, now: u64) -> bool {
        self.fixes.iter().any(|(b, f)| self.fix_ready(b, f, now))
    }

    fn fix_ready(&self, b: &str, f: &FixJob, now: u64) -> bool {
        !self.running.contains_key(b) && f.not_before <= now
    }

    /// A fix that could not start or run goes back, due again after `FIX_RETRY_AFTER`, three times at most.
    pub fn retry_fix(&mut self, branch: &str, f: FixJob, now: u64) -> Retry {
        self.running.remove(branch);
        self.running_fixes.remove(branch);
        if self.fixes.contains_key(branch) { return Retry::Superseded; }
        if f.attempts >= 3 { return Retry::GaveUp; }
        self.fixes.insert(branch.into(), FixJob { attempts: f.attempts + 1, not_before: now + FIX_RETRY_AFTER, ..f });
        Retry::Queued
    }

    /// A fix that was cut short by the usage limit goes back as it was (its attempts are for failures, not for a
    /// pause) and is due again at `until`. A request queued meanwhile stays: it is newer.
    pub fn defer_fix(&mut self, branch: &str, f: FixJob, until: u64) {
        self.running.remove(branch);
        self.running_fixes.remove(branch);
        match self.fixes.get_mut(branch) {
            // A newer request arrived while it ran: merge, as push_fix does, so neither request's findings are lost.
            // The usage limit applies to the newer one too.
            Some(n) => {
                if n.ids.is_empty() || f.ids.is_empty() { n.ids.clear(); }
                else { for id in f.ids { if !n.ids.contains(&id) { n.ids.push(id); } } }
                if n.comment_id.is_none() { n.comment_id = f.comment_id; }
                n.not_before = n.not_before.max(until);
            }
            None => { self.fixes.insert(branch.into(), FixJob { not_before: until, ..f }); }
        }
    }

    pub fn remove(&mut self, branch: &str) {
        self.entries.remove(branch);
    }

    pub fn take_eligible(&mut self, now: u64, debounce: u64) -> Option<(String, Entry)> {
        let b = self.entries.iter()
            .find(|(b, e)| !self.running.contains_key(*b) && now.saturating_sub(e.queued_at) >= debounce)?
            .0.clone();
        let e = self.entries.remove(&b)?;
        self.running.insert(b.clone(), e.clone());
        Some((b, e))
    }

    /// A review that could not start goes back with a full debounce as backoff, three times at most. A newer push
    /// queued meanwhile supersedes it (and any pin it carried) and starts its own count.
    pub fn retry(&mut self, branch: &str, e: Entry, now: u64) -> Retry {
        self.running.remove(branch);
        if self.entries.contains_key(branch) { return Retry::Superseded; }
        if e.attempts >= 3 { return Retry::GaveUp; }
        self.entries.insert(branch.into(), Entry { queued_at: now, attempts: e.attempts + 1, ..e });
        Retry::Queued
    }

    pub fn finish(&mut self, branch: &str) {
        self.running.remove(branch);
        self.running_fixes.remove(branch);
    }

    /// A run that could not complete goes back; a newer queued push keeps its head but widens to the old base.
    pub fn requeue(&mut self, branch: &str, e: Entry, is_ancestor: &dyn Fn(&str, &str) -> bool) {
        self.running.remove(branch);
        match self.entries.get_mut(branch) {
            Some(newer) => if is_ancestor(&e.base, &newer.head) { newer.base = e.base },
            None => { self.entries.insert(branch.into(), e); }
        }
    }

    /// The file as written, for display. `load` is for the daemon: it also requeues interrupted runs.
    pub fn read(path: &Path) -> Queue {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn load(path: &Path) -> Queue {
        let mut q = Queue::read(path);
        for (b, f) in std::mem::take(&mut q.running_fixes) {
            q.running.remove(&b);
            match q.fixes.get_mut(&b) {
                // a newer request for the branch keeps its login; the interrupted one's ids join it
                Some(newer) => {
                    if newer.ids.is_empty() || f.ids.is_empty() { newer.ids.clear(); }
                    else { for id in f.ids { if !newer.ids.contains(&id) { newer.ids.push(id); } } }
                    if newer.comment_id.is_none() { newer.comment_id = f.comment_id; }
                }
                None => { q.fixes.insert(b, f); }
            }
        }
        for (b, e) in std::mem::take(&mut q.running) {
            q.requeue(&b, e, &|_, _| true); // no git here; the next push re-checks ancestry
        }
        q
    }

    pub fn save(&self, path: &Path) {
        crate::status::write_atomic(path, &serde_json::to_vec_pretty(self).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ent(base: &str, head: &str) -> Entry {
        Entry { base: base.into(), head: head.into(), queued_at: 0, model: None, attempts: 0, request: None, whole: false }
    }
    #[test] fn now_reviews_the_whole_branch_even_while_it_runs() {
        let mut q = Queue::default();
        q.push("feat/x", "m", "h", 100, &|_, _| true);
        let _ = q.take_eligible(200, 90).unwrap(); // running m..h
        q.push_now("feat/x", "m", "h", Some("sonnet".into()), None);
        let e = &q.entries["feat/x"];
        assert_eq!((e.base.as_str(), e.head.as_str(), e.model.as_deref(), e.queued_at), ("m", "h", Some("sonnet"), 0));
    }
    #[test] fn only_now_marks_an_entry_whole() {
        let mut q = Queue::default();
        q.push("feat/x", "a", "b", 100, &|_, _| true);
        assert!(!q.entries["feat/x"].whole);
        q.push_now("feat/x", "m", "b", None, None);
        assert!(q.entries["feat/x"].whole);
    }
    #[test] fn now_replaces_a_pending_incremental_range() {
        let mut q = Queue::default();
        q.entries.insert("feat/x".into(), ent("c1", "c2"));
        q.push_now("feat/x", "m", "c2", None, None);
        let e = &q.entries["feat/x"];
        assert_eq!((e.base.as_str(), e.head.as_str(), e.model.is_none()), ("m", "c2", true));
    }
    #[test] fn a_review_that_could_not_start_retries_three_times() {
        let mut q = Queue::default();
        q.push("feat/x", "a", "b", 100, &|_, _| true);
        for n in 1..=3 {
            let (b, e) = q.take_eligible(1000 * n, 90).unwrap();
            assert_eq!(q.retry(&b, e, 1000 * n), Retry::Queued, "attempt {n} goes back");
        }
        let (b, e) = q.take_eligible(9000, 90).unwrap();
        assert_eq!(q.retry(&b, e, 9000), Retry::GaveUp, "the fourth failure gives up");
        assert!(q.entries.is_empty() && q.running.is_empty());
        // A newer push queued meanwhile supersedes the retry (and any pin it carried).
        q.push("feat/x", "a", "b", 9050, &|_, _| true);
        let (b, e) = q.take_eligible(9200, 90).unwrap();
        q.push("feat/x", "b", "c", 9210, &|_, _| true);
        assert_eq!(q.retry(&b, e, 9220), Retry::Superseded);
        q.entries.clear();
        // A new push starts the count again.
        q.push("feat/x", "a", "c", 9100, &|_, _| true);
        assert_eq!(q.entries["feat/x"].attempts, 0);
    }
    #[test] fn a_push_drops_a_pin() {
        let mut q = Queue::default();
        q.entries.insert("feat/x".into(), Entry { model: Some("sonnet".into()), ..ent("a", "b") });
        q.push("feat/x", "b", "c", 300, &|_, _| true);
        assert!(q.entries["feat/x"].model.is_none());
    }
    #[test] fn coalesces_and_debounces() {
        let mut q = Queue::default();
        q.push("feat/x", "a", "b", 100, &|_, _| true);
        q.push("feat/x", "zzz", "c", 150, &|_, _| true);
        assert!(q.take_eligible(200, 90).is_none());
        let (b, e) = q.take_eligible(240, 90).unwrap();
        assert_eq!((b.as_str(), e.base.as_str(), e.head.as_str()), ("feat/x", "a", "c"));
        assert!(q.running.contains_key("feat/x"));
    }
    #[test] fn push_while_running_queues_follow_up_from_running_head() {
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        q.take_eligible(100, 90).unwrap();
        q.push("x", "a", "c", 100, &|a, b| (a, b) != ("b", "a")); // a precedes the running head b
        assert!(q.take_eligible(500, 90).is_none(), "never two runs of one branch");
        q.finish("x");
        let (_, e) = q.take_eligible(500, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("b", "c"));
    }
    #[test] fn a_follow_up_starts_at_a_merge_narrowed_base_that_descends_from_the_running_head() {
        let mut q = Queue::default();
        q.push("x", "a", "r", 0, &|_, _| true);
        q.take_eligible(100, 90).unwrap();
        q.push("x", "m", "h", 100, &|a, b| (a, b) == ("r", "m") || (a, b) == ("r", "h")); // m descends from r
        q.finish("x");
        let (_, e) = q.take_eligible(500, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("m", "h"));
        q.finish("x");
        q.push("x", "a", "r", 600, &|_, _| true);
        q.take_eligible(1000, 90).unwrap();
        q.push("x", "r", "h", 1000, &|_, _| true); // base equal to the running head stays it
        q.finish("x");
        let (_, e) = q.take_eligible(2000, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("r", "h"));
    }
    #[test] fn force_push_replaces_a_base_that_is_no_longer_an_ancestor() {
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        q.push("x", "m", "c", 10, &|_, _| false); // rebased: a is not an ancestor of c
        let (_, e) = q.take_eligible(1000, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("m", "c"));
        q.push("x", "m2", "d", 1000, &|_, _| false); // behind a running review of c, but rebased again
        q.finish("x");
        let (_, e) = q.take_eligible(2000, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("m2", "d"));
    }
    #[test] fn requeue_only_widens_to_an_ancestor_base() {
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        let (_, e) = q.take_eligible(100, 90).unwrap();
        q.push("x", "m", "c", 100, &|_, _| false);
        q.requeue("x", e, &|_, _| false);
        let (_, e2) = q.take_eligible(1000, 90).unwrap();
        assert_eq!(e2.base, "m");
    }
    #[test] fn remove_drops_deleted_branch() {
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        q.remove("x");
        assert!(q.take_eligible(1000, 0).is_none());
    }
    #[test] fn requeue_keeps_newer_head() {
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        let (_, e) = q.take_eligible(100, 90).unwrap();
        q.push("x", "a", "c", 100, &|_, _| true);
        q.requeue("x", e, &|_, _| true);
        let (_, e2) = q.take_eligible(1000, 90).unwrap();
        assert_eq!((e2.base.as_str(), e2.head.as_str()), ("a", "c"));
    }
    #[test] fn read_shows_the_file_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push("x", "a", "b", 0, &|_, _| true);
        q.take_eligible(100, 90).unwrap();
        q.save(&p);
        let r = Queue::read(&p);
        assert!(r.entries.is_empty() && r.running.contains_key("x"));
    }
    #[test] fn load_after_restart_requeues_running() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push("feat/x", "a", "b", 0, &|_, _| true);
        q.take_eligible(100, 90).unwrap();
        q.push("feat/x", "a", "c", 100, &|_, _| true); // follow-up based on b
        q.save(&p);
        let mut q2 = Queue::load(&p);
        assert!(q2.running.is_empty());
        let (_, e) = q2.take_eligible(1000, 90).unwrap();
        assert_eq!((e.base.as_str(), e.head.as_str()), ("a", "c"));
        assert!(Queue::load(&dir.path().join("missing.json")).entries.is_empty());
    }
    fn req(text: &str) -> Request { Request { login: "sam".into(), text: text.into(), comment_id: 7 } }
    #[test] fn a_mention_review_keeps_its_request_through_later_pushes() {
        let mut q = Queue::default();
        q.push_now("feat/x", "m", "h1", None, Some(req("check auth")));
        q.push("feat/x", "h1", "h2", 50, &|_, _| true);
        let (_, e) = q.take_eligible(1000, 90).unwrap();
        assert_eq!((e.head.as_str(), e.request.as_ref().map(|r| r.text.as_str())), ("h2", Some("check auth")));
    }
    #[test] fn fix_requests_merge_and_all_wins() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec!["a".into()], "sam", Some(1), 10);
        q.push_fix("feat/x", vec!["b".into(), "a".into()], "sam", None, 20);
        assert_eq!(q.fixes["feat/x"].ids, vec!["a".to_string(), "b".into()]);
        q.push_fix("feat/x", vec![], "kim", None, 30);
        assert!(q.fixes["feat/x"].ids.is_empty(), "an empty list means every open important finding");
    }
    #[test] fn a_fix_waits_for_a_running_review_and_never_runs_beside_one() {
        let mut q = Queue::default();
        q.push("feat/x", "a", "b", 0, &|_, _| true);
        q.take_eligible(100, 90).unwrap();
        q.push_fix("feat/x", vec![], "sam", None, 100);
        assert!(q.take_fix(0, &|_| Some("b".into())).is_none(), "the branch is being reviewed");
        q.finish("feat/x");
        let (b, f, head) = q.take_fix(0, &|_| Some("b".into())).unwrap();
        assert_eq!((b.as_str(), f.login.as_str(), head.as_str()), ("feat/x", "sam", "b"));
        assert!(q.running.contains_key("feat/x") && q.fixes.is_empty());
        q.push("feat/x", "b", "c", 200, &|_, _| true);
        assert!(q.take_eligible(1000, 90).is_none(), "a review waits for the running fix");
    }
    #[test] fn a_fix_for_a_deleted_branch_is_dropped() {
        let mut q = Queue::default();
        q.push_fix("gone", vec![], "sam", None, 0);
        assert!(q.take_fix(0, &|_| None).is_none());
        assert!(q.fixes.is_empty());
    }
    #[test] fn a_fix_that_could_not_start_retries_three_times() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec![], "sam", None, 0);
        for n in 1..=3 {
            let (b, f, _) = q.take_fix(1000 * n, &|_| Some("h".into())).unwrap();
            assert_eq!(q.retry_fix(&b, f, 1000 * n), Retry::Queued, "attempt {n}");
        }
        let (b, f, _) = q.take_fix(9000, &|_| Some("h".into())).unwrap();
        assert_eq!(q.retry_fix(&b, f, 9000), Retry::GaveUp);
        assert!(q.fixes.is_empty() && q.running.is_empty());
    }
    #[test] fn a_retried_fix_waits_before_it_is_taken_again() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec![], "sam", None, 0);
        assert!(q.fix_due(1000));
        let (b, f, _) = q.take_fix(1000, &|_| Some("h".into())).unwrap();
        assert_eq!(q.retry_fix(&b, f, 1000), Retry::Queued);
        assert!(!q.fix_due(1000) && q.take_fix(1000, &|_| Some("h".into())).is_none(), "never again in the same tick");
        assert!(q.take_fix(1089, &|_| Some("h".into())).is_none());
        assert!(q.fixes.contains_key("feat/x"), "a fix that is not due yet stays queued");
        assert!(q.fix_due(1090) && q.take_fix(1090, &|_| Some("h".into())).is_some());
    }
    #[test] fn a_usage_limit_defers_a_fix_without_spending_an_attempt() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec![], "sam", None, 0);
        for n in 1..=5 {
            let (b, f, _) = q.take_fix(1000 * n, &|_| Some("h".into())).unwrap();
            q.defer_fix(&b, f, 1000 * n + 500);
            assert_eq!(q.fixes["feat/x"].attempts, 0, "pause {n}: no attempt used");
            assert!(q.running.is_empty() && q.running_fixes.is_empty());
            assert!(!q.fix_due(1000 * n + 499), "not before the limit resets");
            assert!(q.fix_due(1000 * n + 500));
        }
        // A request that arrived while it ran replaces the deferred one's place, not the other way round.
        let (b, f, _) = q.take_fix(9000, &|_| Some("h".into())).unwrap();
        q.push_fix("feat/x", vec!["a".into()], "kim", None, 9001);
        q.defer_fix(&b, f, 9500);
        assert_eq!(q.fixes["feat/x"].login, "kim");
    }
    #[test] fn a_deferred_fix_merges_into_a_newer_one_instead_of_being_lost() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec!["aaaaaaaaaaaa".into()], "sam", Some(7), 0);
        let (b, f, _) = q.take_fix(100, &|_| Some("h".into())).unwrap();
        q.push_fix("feat/x", vec!["bbbbbbbbbbbb".into()], "kim", None, 101);
        q.defer_fix(&b, f, 900);
        let j = &q.fixes["feat/x"];
        assert_eq!(j.ids, vec!["bbbbbbbbbbbb".to_string(), "aaaaaaaaaaaa".into()], "both requests survive");
        assert_eq!((j.login.as_str(), j.comment_id), ("kim", Some(7)));
        assert!(!q.fix_due(899) && q.fix_due(900), "the newer one also waits for the usage limit");
    }
    #[test] fn fixes_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push_fix("feat/x", vec!["a".into()], "sam", Some(9), 0);
        q.save(&p);
        assert_eq!(Queue::load(&p).fixes["feat/x"].comment_id, Some(9));
    }
    #[test] fn a_running_fix_comes_back_as_a_fix_not_a_review_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push_fix("feat/x", vec!["a".into()], "sam", Some(9), 0);
        q.take_fix(0, &|_| Some("c".into())).unwrap();
        q.save(&p);
        let q2 = Queue::load(&p);
        assert!(q2.entries.is_empty() && q2.running.is_empty() && q2.running_fixes.is_empty());
        assert_eq!(q2.fixes["feat/x"].ids, vec!["a".to_string()]);
    }
    #[test] fn a_running_fix_never_narrows_a_pending_review() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push_fix("feat/x", vec![], "sam", None, 0);
        q.take_fix(0, &|_| Some("c".into())).unwrap();
        q.push("feat/x", "a", "d", 10, &|_, _| false); // rebased: the fix's head c is no ancestor of d
        assert_eq!(q.entries["feat/x"].base, "a");
        q.save(&p);
        let q2 = Queue::load(&p);
        assert_eq!((q2.entries["feat/x"].base.as_str(), q2.entries["feat/x"].head.as_str()), ("a", "d"));
        assert!(q2.running.is_empty() && q2.fixes.contains_key("feat/x"));
    }
    #[test] fn a_restarted_fix_merges_with_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("queue.json");
        let mut q = Queue::default();
        q.push_fix("feat/x", vec!["a".into()], "sam", None, 0);
        q.take_fix(0, &|_| Some("c".into())).unwrap();
        q.push_fix("feat/x", vec!["b".into()], "kim", None, 5);
        q.save(&p);
        let f = &Queue::load(&p).fixes["feat/x"];
        assert_eq!(f.ids, vec!["b".to_string(), "a".into()]);
    }
    #[test] fn finish_clears_a_running_fix() {
        let mut q = Queue::default();
        q.push_fix("feat/x", vec![], "sam", None, 0);
        q.take_fix(0, &|_| Some("c".into())).unwrap();
        assert!(q.running_fixes.contains_key("feat/x"));
        q.finish("feat/x");
        assert!(q.running_fixes.is_empty() && q.running.is_empty());
    }
}
