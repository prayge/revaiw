use crate::model::{Finding, ReviewOutput};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LedgerEntry {
    #[serde(flatten)]
    pub f: Finding,
    pub status: String, // open | fixed | recheck | dismissed | deferred
    pub first_seen_sha: String,
    pub last_seen_sha: String,
    #[serde(default)]
    pub decision_reason: Option<String>,
    #[serde(default)]
    pub comment_id: Option<u64>,
}

/// Every finding a branch has had, across pushes.
#[derive(Serialize, Deserialize, Default)]
pub struct Ledger {
    pub findings: BTreeMap<String, LedgerEntry>,
    /// Reviews merged so far; after the first, follow-ups converge (see `converge`).
    #[serde(default)]
    pub reviews: u32,
}

pub struct Merged {
    pub new: Vec<String>,
    pub fixed: Vec<String>,
}

/// Re-review convergence: a follow-up review (see `Ledger::follow_up`) reports important findings only, so
/// fix-up pushes don't keep producing fresh nits.
pub fn converge(follow_up: bool, out: &mut ReviewOutput) {
    if follow_up {
        out.findings.retain(|f| f.severity == "important");
    }
}

/// CI failures already broken on the base branch stay visible as `pre_existing`, so the agent working this
/// branch is not told to fix someone else's code.
pub fn demote_outside(out: &mut ReviewOutput, changed: &std::collections::HashSet<String>) {
    // Both are required: a file outside the diff can still be a regression (an unchanged caller broken by a
    // changed signature), so the path alone never demotes; the model's on_base claim alone is not trusted either.
    for f in out.findings.iter_mut().filter(|f| f.severity == "important" && f.on_base && !changed.contains(&f.file)) {
        f.severity = "pre_existing".into();
    }
}

/// FNV-1a over file + normalised summary, so a re-reported issue lands on the same entry.
pub fn finding_id(f: &Finding) -> String {
    let norm: String = f.summary.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in f.file.bytes().chain([0u8]).chain(norm.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..12].to_string()
}

impl Ledger {
    pub fn load(path: &Path) -> Ledger {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
        crate::status::write_atomic(path, &serde_json::to_vec_pretty(self).unwrap());
    }

    /// A push is a follow-up (presumably fixing) only while earlier findings are open or parked: new work
    /// after a clean review gets a full review, nits included.
    pub fn follow_up(&self) -> bool {
        !self.to_check().is_empty()
    }

    /// The PR comments of findings settled in the ledger (fixed, dismissed): their threads get resolved. Deferred
    /// is postponed, not settled, so its thread stays open.
    pub fn closed_comment_ids(&self) -> std::collections::HashSet<u64> {
        self.comment_ids(|s| matches!(s, "fixed" | "dismissed"))
    }

    /// The PR comments of findings still live (open, recheck, deferred): a resolved thread of one is reopened.
    pub fn open_comment_ids(&self) -> std::collections::HashSet<u64> {
        self.comment_ids(|s| matches!(s, "open" | "recheck" | "deferred"))
    }

    fn comment_ids(&self, keep: impl Fn(&str) -> bool) -> std::collections::HashSet<u64> {
        self.findings.values().filter(|e| keep(e.status.as_str())).filter_map(|e| e.comment_id).filter(|c| *c != 0).collect()
    }

    /// A person's 👎 (or a triage): a live finding becomes `dismissed` with the reason. False when the id is unknown
    /// or already settled, so a repeated reaction changes nothing.
    pub fn dismiss(&mut self, id: &str, reason: &str) -> bool {
        match self.findings.get_mut(id) {
            Some(e) if matches!(e.status.as_str(), "open" | "recheck" | "deferred") => {
                e.status = "dismissed".into();
                e.decision_reason = Some(reason.into());
                true
            }
            _ => false,
        }
    }

    /// After revaiw pushed a fix: the open findings it addressed become `fixed`; the next review verifies them.
    pub fn mark_fixed(&mut self, ids: &[String], reason: &str, sha: &str) {
        for id in ids {
            if let Some(e) = self.findings.get_mut(id).filter(|e| e.status == "open") {
                e.status = "fixed".into();
                e.decision_reason = Some(reason.into());
                e.last_seen_sha = sha.into();
            }
        }
    }

    pub fn open(&self) -> Vec<(String, LedgerEntry)> {
        self.findings.iter().filter(|(_, e)| e.status == "open").map(|(k, e)| (k.clone(), e.clone())).collect()
    }

    /// What the next review is asked about: the open findings plus any parked as `recheck`. A `recheck` is a
    /// fix that a review of a superseded head re-reported; it is never counted or shown, only re-asked.
    pub fn to_check(&self) -> Vec<(String, LedgerEntry)> {
        self.findings.iter().filter(|(_, e)| matches!(e.status.as_str(), "open" | "recheck"))
            .map(|(k, e)| (k.clone(), e.clone())).collect()
    }

    /// Triage decisions (dismissed/deferred) are never overwritten by a later run.
    pub fn merge(&mut self, out: &ReviewOutput, sha: &str) -> Merged {
        self.merge_with(out, sha, false)
    }

    /// A review of a head the branch has already moved past (a push landed while it ran). It may re-report
    /// what a newer commit fixed, so such an entry is parked as `recheck`: not open (never counted, shown or
    /// posted) but in `to_check`, so every following review is asked about it — even after skipped or failed
    /// ones — until a current review answers: resolved → fixed, still open or re-reported → open.
    pub fn merge_superseded(&mut self, out: &ReviewOutput, sha: &str) -> Merged {
        self.merge_with(out, sha, true)
    }

    fn merge_with(&mut self, out: &ReviewOutput, sha: &str, superseded: bool) -> Merged {
        let mut m = Merged { new: vec![], fixed: vec![] };
        self.reviews += 1;
        for id in &out.resolved {
            if let Some(e) = self.findings.get_mut(id) {
                match e.status.as_str() {
                    "open" => { e.status = "fixed".into(); e.last_seen_sha = sha.into(); m.fixed.push(id.clone()); }
                    "recheck" if !superseded => { e.status = "fixed".into(); e.last_seen_sha = sha.into(); }
                    _ => {}
                }
            }
        }
        for id in &out.still_open {
            if let Some(e) = self.findings.get_mut(id) {
                e.last_seen_sha = sha.into();
                if e.status == "recheck" && !superseded { e.status = "open".into(); }
            }
        }
        for f in &out.findings {
            let id = finding_id(f);
            match self.findings.get_mut(&id) {
                Some(e) => {
                    e.f = f.clone();
                    e.last_seen_sha = sha.into();
                    // A fixed finding re-reported by a review of a superseded head is parked as `recheck`
                    // (the push that superseded it likely fixed it); a current review reopens it.
                    if matches!(e.status.as_str(), "fixed" | "recheck") {
                        e.status = if superseded { "recheck" } else { "open" }.into();
                    }
                }
                None => {
                    self.findings.insert(id.clone(), LedgerEntry {
                        f: f.clone(), status: "open".into(), first_seen_sha: sha.into(),
                        last_seen_sha: sha.into(), decision_reason: None, comment_id: None,
                    });
                    m.new.push(id);
                }
            }
        }
        // A `recheck` the review did not mention is not known fixed (an omission, or a re-report converge dropped
        // as a nit): it stays parked and is asked about again — like an unmentioned `open` stays open.
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fnd(file: &str, summary: &str) -> Finding {
        Finding { severity: "important".into(), file: file.into(), line: 1, end_line: None, summary: summary.into(),
                  why: "w".into(), snippet: String::new(), lang: String::new(), suggested_patch: None, suggestion: None, on_base: false }
    }
    #[test] fn id_is_stable_and_whitespace_case_insensitive() {
        assert_eq!(finding_id(&fnd("a.ts", "Missing  RLS")), finding_id(&fnd("a.ts", "missing rls")));
        assert_ne!(finding_id(&fnd("a.ts", "x")), finding_id(&fnd("b.ts", "x")));
        assert_eq!(finding_id(&fnd("a.ts", "x")).len(), 12);
    }
    #[test] fn merge_new_resolved_reported_again() {
        let mut l = Ledger::default();
        let a = fnd("a.ts", "bug a");
        let id = finding_id(&a);
        let m = l.merge(&ReviewOutput { findings: vec![a.clone()], ..Default::default() }, "s1");
        assert_eq!(m.new, vec![id.clone()]);
        assert_eq!(l.open().len(), 1);
        let m = l.merge(&ReviewOutput { resolved: vec![id.clone(), "unknown".into()], ..Default::default() }, "s2");
        assert_eq!(m.fixed, vec![id.clone()]);
        assert_eq!(l.findings[&id].status, "fixed");
        l.merge(&ReviewOutput { findings: vec![a], ..Default::default() }, "s3");
        assert_eq!(l.findings[&id].status, "open");
        assert_eq!(l.findings[&id].first_seen_sha, "s1");
        assert_eq!(l.findings[&id].last_seen_sha, "s3");
    }
    fn fixed_entry(l: &mut Ledger, file: &str, summary: &str) -> (Finding, String) {
        let a = fnd(file, summary);
        let id = finding_id(&a);
        l.merge(&ReviewOutput { findings: vec![a.clone()], ..Default::default() }, "s1");
        l.findings.get_mut(&id).unwrap().status = "fixed".into();
        (a, id)
    }
    #[test] fn a_superseded_review_parks_a_re_reported_fix_as_recheck() {
        // A push landed while the review ran, so it may re-report what that push fixed: park it as `recheck` —
        // not open (never counted, shown or posted) — and still record genuinely new findings.
        let mut l = Ledger::default();
        let (a, id) = fixed_entry(&mut l, "a.ts", "bug a");
        let b = fnd("b.ts", "bug b");
        let m = l.merge_superseded(&ReviewOutput { findings: vec![a, b.clone()], ..Default::default() }, "s0");
        assert_eq!(l.findings[&id].status, "recheck");
        assert!(l.open().iter().all(|(i, _)| *i != id), "not in the open list: not counted or posted");
        assert!(l.to_check().iter().any(|(i, _)| *i == id), "but the next review is asked about it");
        assert_eq!(m.new, vec![finding_id(&b)]);
        // Another superseded review leaves it parked.
        l.merge_superseded(&ReviewOutput { still_open: vec![id.clone()], ..Default::default() }, "s0b");
        assert_eq!(l.findings[&id].status, "recheck");
    }
    #[test] fn the_next_current_review_settles_a_recheck() {
        for (out, want) in [
            // Silent: not known fixed, so it stays parked and is asked about again (never lost, never an alarm).
            ("resolved", "fixed"), ("still_open", "open"), ("reported", "open"), ("silent", "recheck"),
        ] {
            let mut l = Ledger::default();
            let (a, id) = fixed_entry(&mut l, "a.ts", "bug a");
            l.merge_superseded(&ReviewOutput { findings: vec![a.clone()], ..Default::default() }, "s0");
            let o = match out {
                "resolved" => ReviewOutput { resolved: vec![id.clone()], ..Default::default() },
                "still_open" => ReviewOutput { still_open: vec![id.clone()], ..Default::default() },
                "reported" => ReviewOutput { findings: vec![a], ..Default::default() },
                _ => ReviewOutput::default(),
            };
            l.merge(&o, "s2");
            assert_eq!(l.findings[&id].status, want, "{out}");
        }
    }
    #[test] fn triage_decisions_survive_re_reports() {
        let mut l = Ledger::default();
        let a = fnd("a.ts", "bug a");
        let id = finding_id(&a);
        l.merge(&ReviewOutput { findings: vec![a.clone()], ..Default::default() }, "s1");
        l.findings.get_mut(&id).unwrap().status = "dismissed".into();
        l.merge(&ReviewOutput { findings: vec![a], still_open: vec![id.clone()], ..Default::default() }, "s2");
        assert_eq!(l.findings[&id].status, "dismissed");
        assert!(l.open().is_empty());
    }
    #[test] fn closed_comments_are_the_fixed_and_dismissed_ones() {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![fnd("a.ts", "x"), fnd("b.ts", "y"), fnd("c.ts", "z"), fnd("d.ts", "w")],
                                ..Default::default() }, "s");
        let ids: Vec<String> = l.findings.keys().cloned().collect();
        for (n, (id, status)) in ids.iter().zip(["fixed", "dismissed", "open", "deferred"]).enumerate() {
            let e = l.findings.get_mut(id).unwrap();
            e.status = status.into();
            e.comment_id = Some(n as u64 + 10);
        }
        // Deferred is postponed, not settled: its thread stays open.
        let closed = l.closed_comment_ids();
        assert_eq!(closed.len(), 2);
        assert!(closed.contains(&10) && closed.contains(&11) && !closed.contains(&12) && !closed.contains(&13));
        let open = l.open_comment_ids();
        assert!(open.contains(&12) && open.contains(&13) && open.len() == 2);
    }
    #[test] fn follow_up_only_while_findings_are_open() {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput::default(), "a");
        assert!(!l.follow_up(), "a clean first review leaves the next push a full review");
        l.merge(&ReviewOutput { findings: vec![fnd("a.ts", "bug")], ..Default::default() }, "b");
        assert!(l.follow_up());
    }
    #[test] fn follow_up_reviews_keep_only_important() {
        let mut l = Ledger::default();
        let mut first = ReviewOutput { findings: vec![fnd("a.ts", "bug"), Finding { severity: "nit".into(), ..fnd("a.ts", "style") }],
                                       ..Default::default() };
        converge(l.follow_up(), &mut first);
        assert_eq!(first.findings.len(), 2, "first review keeps nits");
        l.merge(&first, "s1");
        assert!(l.follow_up(), "the first review left findings open");
        let mut next = ReviewOutput { findings: vec![fnd("b.ts", "bug2"), Finding { severity: "nit".into(), ..fnd("b.ts", "style2") },
                                                     Finding { severity: "pre_existing".into(), ..fnd("c.ts", "old") }],
                                      ..Default::default() };
        converge(l.follow_up(), &mut next);
        assert_eq!(next.findings.iter().map(|f| f.severity.as_str()).collect::<Vec<_>>(), vec!["important"]);
    }
    #[test] fn ci_findings_outside_the_pr_are_pre_existing() {
        let on_base = |f: Finding| Finding { on_base: true, ..f };
        let mut out = ReviewOutput { findings: vec![
            fnd("mine.ts", "broke"),                                   // in the diff
            on_base(fnd("base.ts", "already broken on uat")),          // outside the diff, model confirmed: demote
            fnd("caller.ts", "unchanged caller broken by my signature"), // outside the diff, a regression: keep
            on_base(fnd("mine.ts", "model says base, but file changed")), // in the diff: keep
        ], ..Default::default() };
        let changed: std::collections::HashSet<String> = ["mine.ts".to_string()].into();
        demote_outside(&mut out, &changed);
        assert_eq!(out.findings.iter().map(|f| f.severity.as_str()).collect::<Vec<_>>(),
                   vec!["important", "pre_existing", "important", "important"]);
    }
    #[test] fn roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("feat/x/ledger.json");
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![fnd("a.ts", "s")], ..Default::default() }, "s1");
        l.save(&p);
        assert_eq!(Ledger::load(&p).findings.len(), 1);
    }
    #[test] fn a_dismissal_settles_a_live_finding_once() {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![fnd("a.ts", "x")], ..Default::default() }, "s1");
        let id = l.findings.keys().next().unwrap().clone();
        assert!(l.dismiss(&id, "👎 by kim"));
        assert_eq!((l.findings[&id].status.as_str(), l.findings[&id].decision_reason.as_deref()), ("dismissed", Some("👎 by kim")));
        assert!(!l.dismiss(&id, "again"), "already settled");
        assert!(!l.dismiss("nope", "x"));
    }
    #[test] fn mark_fixed_settles_open_findings_only() {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![fnd("a.ts", "x"), fnd("b.ts", "y")], ..Default::default() }, "s1");
        let ids: Vec<String> = l.findings.keys().cloned().collect();
        l.dismiss(&ids[1], "no");
        l.mark_fixed(&ids, "revaiw fix abc1234", "abc1234");
        assert_eq!(l.findings[&ids[0]].status, "fixed");
        assert_eq!(l.findings[&ids[1]].status, "dismissed");
    }
}
