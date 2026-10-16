use crate::ledger::{Ledger, LedgerEntry};
use crate::model::{Finding, ReviewOutput};
use std::fmt::Write;

pub const MARKER: &str = "<!-- revaiw -->";

pub struct Ctx<'a> {
    pub title: &'a str,
    pub branch: &'a str,
    pub base: &'a str,
    pub head: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub reason: &'a str,
    pub out: &'a ReviewOutput,
    pub ledger: &'a Ledger,
    pub fixed_now: &'a [String],
    /// The engine shown in the footer, e.g. `/code-review medium`; None for a CI diagnosis.
    pub engine: Option<&'a str>,
}

/// A backtick fence one longer than the longest run inside `body`, so snippets can't close it early.
pub fn fence(body: &str) -> String {
    let (mut run, mut max) = (0, 0);
    for c in body.chars() {
        if c == '`' { run += 1; max = max.max(run) } else { run = 0 }
    }
    "`".repeat((max + 1).max(3))
}

fn block(lang: &str, body: &str) -> String {
    let f = fence(body);
    format!("{f}{lang}\n{body}\n{f}\n")
}

fn short(sha: &str) -> &str { &sha[..sha.len().min(7)] }

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

fn finding_block(e: &LedgerEntry, open: bool) -> String {
    let f = &e.f;
    let mut s = format!("<details{}><summary><code>{}:{}</code> — {}</summary>\n\n",
                        if open { " open" } else { "" }, f.file, f.line, f.summary);
    if !f.snippet.is_empty() { s += &block(&f.lang, &f.snippet); }
    let _ = write!(s, "\n**Why:** {}\n", f.why);
    if let Some(p) = &f.suggested_patch {
        s += "\n**Suggested change**\n";
        s += &block("diff", p);
    }
    s + "</details>\n\n"
}

/// The sticky PR comment (and the local .md): every open finding on the branch, not just this run's.
pub fn summary(c: &Ctx) -> String {
    let open = c.ledger.open();
    let of = |sev: &str| open.iter().filter(|(_, e)| e.f.severity == sev).map(|(_, e)| e).collect::<Vec<_>>();
    let (imp, nits, pre) = (of("important"), of("nit"), of("pre_existing"));
    let mut s = format!("{MARKER}\n## {} · `{}` @ `{}`\n", c.title, c.branch, short(c.head));
    if c.out.partial() {
        let _ = write!(s, "**⚠️ Partial review: /code-review {}**", c.out.gaps());
        if !open.is_empty() {
            let _ = write!(s, " · {} important · {} · {} pre-existing", imp.len(), plural(nits.len(), "nit"), pre.len());
        }
    } else if open.is_empty() {
        s += "No open findings.";
    } else {
        let _ = write!(s, "**{} · {} · {}**", format!("{} important", imp.len()), plural(nits.len(), "nit"),
                       format!("{} pre-existing", pre.len()));
    }
    if !c.fixed_now.is_empty() { let _ = write!(s, " — {} fixed since last push", c.fixed_now.len()); }
    s += "\n\n";
    if !c.out.summary.is_empty() { let _ = writeln!(s, "{}\n", c.out.summary); }
    if !c.out.refused.is_empty() {
        s += "<details open><summary>Refused</summary>\n\n";
        for u in &c.out.refused { let _ = writeln!(s, "- `{}`", crate::review::public_refusal(u)); }
        s += "\n</details>\n\n";
    }
    if !c.out.walkthrough.is_empty() {
        let _ = write!(s, "<details><summary>Walkthrough ({})</summary>\n\n| File | Change |\n|---|---|\n",
                       plural(c.out.walkthrough.len(), "file"));
        for w in &c.out.walkthrough { let _ = writeln!(s, "| `{}` | {} |", w.file, w.change.replace('|', "\\|")); }
        s += "</details>\n\n";
    }
    if !imp.is_empty() {
        s += "### 🔴 Important\n";
        for e in &imp { s += &finding_block(e, true); }
    }
    for (list, label) in [(&nits, "🟡 ".to_string() + &plural(nits.len(), "nit")),
                          (&pre, "🟣 ".to_string() + &plural(pre.len(), "pre-existing issue"))] {
        if list.is_empty() { continue; }
        let _ = write!(s, "<details><summary>{label}</summary>\n\n");
        for e in list.iter() { s += &finding_block(e, false); }
        s += "</details>\n\n";
    }
    if !c.fixed_now.is_empty() {
        let _ = write!(s, "<details><summary>✅ Fixed since last review ({})</summary>\n\n", c.fixed_now.len());
        for id in c.fixed_now {
            if let Some(e) = c.ledger.findings.get(id) { let _ = writeln!(s, "- `{}:{}` {}", e.f.file, e.f.line, e.f.summary); }
        }
        s += "</details>\n\n";
    }
    match c.engine {
        Some(e) => { let _ = write!(s, "<sub>revaiw (with {e}) · {} ({}) · {}..{}</sub>\n",
                                    c.model, c.reason, short(c.base), short(c.head)); }
        None => { let _ = write!(s, "<sub>revaiw · {}/{} ({}) · {}..{}</sub>\n",
                                 c.model, c.effort, c.reason, short(c.base), short(c.head)); }
    }
    s
}

/// The PR comment for a fix job: what was pushed, or why nothing was, with the patch so a person can apply it.
pub fn fix_comment(branch: &str, head: &str, pushed: Option<&str>, items: &[(String, LedgerEntry)], login: &str,
                   failure: Option<(&str, &str)>) -> String {
    let mut s = format!("{MARKER}\n## 🔧 Fix · `{branch}` @ `{}`\n\n", short(head));
    match (pushed, failure) {
        (Some(sha), _) => { let _ = writeln!(s, "Pushed `{}`, requested by @{login}, fixing:\n", short(sha)); }
        (None, Some((why, _))) => { let _ = writeln!(s, "**Not pushed:** {why}. Requested by @{login}. Findings:\n"); }
        (None, None) => { let _ = writeln!(s, "Not pushed. Requested by @{login}. Findings:\n"); }
    }
    for (id, e) in items { let _ = writeln!(s, "- `{id}` `{}:{}` {}", e.f.file, e.f.line, e.f.summary); }
    if let Some((_, patch)) = failure.filter(|(_, p)| !p.is_empty()) {
        s += "\n<details><summary>The attempted change</summary>\n\n";
        s += &block("diff", patch);
        s += "</details>\n";
    }
    s + "\n<sub>revaiw fix · the next review verifies it</sub>\n"
}

/// Inline review comment body; a `suggestion` block gives GitHub's one-click "Commit suggestion".
pub fn inline_body(f: &Finding, with_suggestion: bool) -> String {
    let mut s = format!("**🔴 {}**\n\n{}\n", f.summary, f.why);
    if let Some(sug) = f.suggestion.as_ref().filter(|_| with_suggestion) {
        s += "\n";
        s += &block("suggestion", sug);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Finding, Walk};
    fn fnd(sev: &str, summary: &str) -> Finding {
        Finding { severity: sev.into(), file: "apps/a.ts".into(), line: 42, end_line: None, summary: summary.into(),
                  why: "because".into(), snippet: "const a = ```x```".into(), lang: "ts".into(),
                  suggested_patch: Some("- a\n+ b".into()), suggestion: Some("const a = 1".into()), on_base: false }
    }
    #[test] fn fence_outgrows_content() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("has ``` inside"), "````");
        assert_eq!(fence("has ````` five"), "``````");
    }
    #[test] fn summary_shape() {
        let mut l = Ledger::default();
        let out = ReviewOutput {
            findings: vec![fnd("important", "skips validation"), fnd("nit", "naming"), fnd("pre_existing", "old bug")],
            walkthrough: vec![Walk { file: "apps/a.ts".into(), change: "adds x".into() }],
            summary: "One real bug.".into(), ..Default::default()
        };
        l.merge(&out, "def5678");
        let s = summary(&Ctx { title: "🐇 Review", branch: "feat/x", base: "abc1234aaaa", head: "def5678bbbb", model: "claude-haiku-5-5",
                               effort: "high", reason: "small", out: &out, ledger: &l, fixed_now: &["k".into()], engine: None });
        assert!(s.starts_with(MARKER));
        assert!(s.contains("## 🐇 Review · `feat/x` @ `def5678`"));
        assert!(s.contains("**1 important · 1 nit · 1 pre-existing** — 1 fixed since last push"));
        assert!(s.contains("<details><summary>Walkthrough (1 file)</summary>"));
        assert!(s.contains("### 🔴 Important"));
        assert!(s.contains("<details open><summary><code>apps/a.ts:42</code> — skips validation</summary>"));
        assert!(s.contains("````ts\nconst a = ```x```\n````"));
        assert!(s.contains("```diff\n- a\n+ b\n```"));
        assert!(s.contains("<details><summary>🟡 1 nit</summary>"));
        assert!(s.contains("<sub>revaiw · claude-haiku-5-5/high (small) · abc1234..def5678</sub>"));
    }
    #[test] fn zero_pre_existing_is_not_pluralised() {
        let mut l = Ledger::default();
        let out = ReviewOutput { findings: vec![fnd("important", "x")], ..Default::default() };
        l.merge(&out, "h");
        let s = summary(&Ctx { title: "🐇 Review", branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                               out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(s.contains("**1 important · 0 nits · 0 pre-existing**"), "{s}");
    }
    #[test] fn ci_runs_get_their_own_title() {
        let l = Ledger::default();
        let out = ReviewOutput::default();
        let s = summary(&Ctx { title: "🧪 CI failures", branch: "b", base: "h", head: "h", model: "m", effort: "e",
                               reason: "r", out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(s.contains("## 🧪 CI failures · `b` @ `h`"), "{s}");
    }
    #[test] fn important_is_never_pluralised() {
        let mut l = Ledger::default();
        let mut b = fnd("important", "y");
        b.file = "apps/b.ts".into();
        let out = ReviewOutput { findings: vec![fnd("important", "x"), b], ..Default::default() };
        l.merge(&out, "h");
        let s = summary(&Ctx { title: "t", branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                               out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(s.contains("**2 important · 0 nits"), "{s}");
    }
    #[test] fn clean_run_says_so() {
        let l = Ledger::default();
        let out = ReviewOutput::default();
        let s = summary(&Ctx { title: "🐇 Review", branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                               out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(s.contains("No open findings."));
    }
    #[test] fn partial_review_never_reads_as_clean() {
        let l = Ledger::default();
        let out = ReviewOutput { refused: vec!["Bash: git blame a.rs".into(), "Bash: find . -name x".into()], ..Default::default() };
        let s = summary(&Ctx { title: "🐇 Review", branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                               out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(!s.contains("No open findings."), "{s}");
        assert!(s.contains("⚠️ Partial review: /code-review was refused 2 reads"), "{s}");
        // Published as program names only: arguments can carry secrets.
        assert!(s.contains("- `Bash: git`\n- `Bash: find`") && !s.contains("blame a.rs"), "{s}");
    }
    #[test] fn the_footer_says_code_review_ran() {
        let l = Ledger::default();
        let out = ReviewOutput::default();
        let s = summary(&Ctx { title: "🐇 Review", branch: "b", base: "abc1234", head: "def5678", model: "claude-sonnet-5-5",
                               effort: "medium", reason: "412 lines", out: &out, ledger: &l, fixed_now: &[],
                               engine: Some("/code-review medium") });
        assert!(s.contains("<sub>revaiw (with /code-review medium) · claude-sonnet-5-5 (412 lines) · abc1234..def5678</sub>"), "{s}");
    }
    #[test] fn an_incomplete_review_says_so_and_claims_no_refusal() {
        let l = Ledger::default();
        let out = ReviewOutput { incomplete: true, ..Default::default() };
        let s = summary(&Ctx { title: "🐇 Review", branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                               out: &out, ledger: &l, fixed_now: &[], engine: None });
        assert!(s.contains("⚠️ Partial review: /code-review did not complete the review") && !s.contains("refused"), "{s}");
    }
    #[test] fn a_fix_comment_says_what_was_pushed_or_why_not() {
        let e = crate::ledger::LedgerEntry { f: fnd("important", "double charge"), status: "open".into(),
            first_seen_sha: "s".into(), last_seen_sha: "s".into(), decision_reason: None, comment_id: None };
        let items = vec![("0123456789ab".to_string(), e)];
        let ok = fix_comment("feat/x", "abc1234aaa", Some("def5678bbb"), &items, "kim", None);
        assert!(ok.starts_with(MARKER) && ok.contains("def5678") && ok.contains("0123456789ab") && ok.contains("@kim"));
        let no = fix_comment("feat/x", "abc1234aaa", None, &items, "kim", Some(("the fix changed c.ts", "--- a/c.ts\n")));
        assert!(no.contains("Not pushed") && no.contains("the fix changed c.ts") && no.contains("```diff"), "{no}");
    }
    #[test] fn inline_has_suggestion_block() {
        let b = inline_body(&fnd("important", "skips validation"), true);
        assert!(b.contains("**🔴 skips validation**"));
        assert!(b.contains("```suggestion\nconst a = 1\n```"));
    }
}
