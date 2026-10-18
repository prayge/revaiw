//! `revaiw report [hours]`: what the daemon did over the last hours, read back from `daemon.log` (the same lines
//! `revaiw watch` streams): results, failures, refused commands, errors and timings.

#[derive(Default, Debug)]
pub struct Report {
    pub reviews: usize,
    pub clean: usize,
    pub important: usize,
    /// Reviews that could not read or check something; one can also have important findings.
    pub partial: usize,
    pub ci: usize,
    pub ready: usize,
    pub holds: usize,
    pub usage_limits: usize,
    pub failed: Vec<String>,
    pub refused: Vec<String>,
    /// Each distinct error line, with how often it came, in first-seen order.
    pub errors: Vec<(String, usize)>,
    pub review_secs: Vec<u64>,
    pub wait_secs: Vec<u64>,
}

/// The number before `s` that follows `label` (`"wait 92s"` → 92).
fn secs(msg: &str, label: &str) -> Option<u64> {
    let rest = &msg[msg.find(label)? + label.len()..];
    rest[..rest.find('s')?].parse().ok()
}

/// What the log lines from `since` (epoch seconds) on say. Lines that are not `[epoch] message` are skipped.
pub fn summarize(log: &str, since: u64) -> Report {
    let mut r = Report::default();
    for l in log.lines() {
        let Some((t, msg)) = l.strip_prefix('[').and_then(|l| l.split_once("] ")) else { continue };
        if t.parse::<u64>().map_or(true, |t| t < since) { continue; }
        let result = |kind: &str| msg.starts_with(kind) && msg.contains('@') && msg.contains(" → ");
        if result("review ") {
            r.reviews += 1;
            if msg.contains(": 🔴 ") { r.important += 1; }
            if msg.contains(": clean (") { r.clean += 1; }
            r.review_secs.extend(secs(msg, "· review ").or_else(|| secs(msg, "⏱ review ")));
            r.wait_secs.extend(secs(msg, "⏱ wait "));
        } else if result("ci ") {
            r.ci += 1;
        } else if msg.contains(": ⚠️ partial review @") {
            r.partial += 1;
        } else if msg.contains(": ✅ ready to merge @") {
            r.ready += 1;
        } else if msg.starts_with("holding: ") {
            r.holds += 1;
        } else if msg.contains(": usage limit, paused") {
            r.usage_limits += 1;
        } else if let Some((b, cmd)) = msg.split_once(": /code-review was refused ") {
            r.refused.push(format!("{b}: {cmd}"));
        } else if msg.contains(" failed: ") {
            r.failed.push(msg.to_string());
        } else if ["inbox: error", ": comment: ", ": fix comment: ", ": inline review: ", ": sync PR: "]
            .iter().any(|e| msg.contains(e)) {
            match r.errors.iter_mut().find(|(m, _)| m == msg) {
                Some((_, n)) => *n += 1,
                None => r.errors.push((msg.to_string(), 1)),
            }
        }
    }
    r
}

fn median(v: &[u64]) -> u64 {
    let mut v = v.to_vec();
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

/// One line per list entry, cut to 160 characters (a refused command can be a whole script).
fn short(s: &str) -> String {
    if s.chars().count() <= 160 { s.to_string() } else { s.chars().take(160).collect::<String>() + "…" }
}

pub fn render(r: &Report, hours: u64) -> String {
    let mut out = format!("revaiw report · last {hours}h\n");
    if r.reviews == 0 { out += &format!("  no reviews in the last {hours}h\n"); } else {
        out += &format!("  reviews {}: {} clean · {} with important findings · {} ⚠️ partial\n",
                        r.reviews, r.clean, r.important, r.partial);
        out += &format!("  review time median {}s (max {}s) · queue wait median {}s\n", median(&r.review_secs),
                        r.review_secs.iter().max().unwrap_or(&0), median(&r.wait_secs));
    }
    out += &format!("  ci diagnoses {} · ✅ ready to merge {}\n", r.ci, r.ready);
    out += &format!("  usage-limit pauses {} · resource-guard holds {}\n", r.usage_limits, r.holds);
    if !r.refused.is_empty() {
        out += &format!("  refused commands ({}), each made a review partial:\n", r.refused.len());
        for c in &r.refused { out += &format!("    {}\n", short(c)); }
    }
    if !r.failed.is_empty() {
        out += &format!("  failed ({}):\n", r.failed.len());
        for f in &r.failed { out += &format!("    {}\n", short(f)); }
    }
    if !r.errors.is_empty() {
        out += &format!("  errors ({} kind{}):\n", r.errors.len(), if r.errors.len() == 1 { "" } else { "s" });
        for (e, n) in &r.errors { out += &format!("    {n}× {}\n", short(e)); }
    }
    out
}

/// `revaiw report [hours]`: the window, 24 h unless given.
pub fn hours_arg(args: &[String]) -> Result<u64, String> {
    match args.first() {
        None => Ok(24),
        Some(a) => a.parse().ok().filter(|h| *h > 0).ok_or_else(|| format!("usage: revaiw report [hours], got {a:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LOG: &str = "\
[100] started pid 1 watching /r/.git/refs/remotes/origin (3 branches, 2 slots)
[1000] review fix/a 2f3337b..42675c9 on claude-opus-5-5 with /code-review medium (4735 lines)
[1100] fix/a: /code-review was refused Bash: for f in a b; do grep -n x $f; done
[1200] review fix/a@42675c9 #7: 🔴 3 important, 3 nits · ⏱ wait 782s · review 251s · post 7s → /r/.reviews/fix/a/42675c9.md
[1200] fix/a: ⚠️ partial review @42675c9, /code-review was refused 1 read (see the review comment)
[1300] review docs/b@fe60ffa: ⚠️ partial (0 important, 5 nits; was refused 1 read) · ⏱ wait 200s · review 221s · post 5s → /r/x.md
[1300] docs/b: ⚠️ partial review @fe60ffa, /code-review was refused 1 read (see the review comment)
[1400] review uat@f647c6c: clean (0 important, 1 nits), waiting for CI · ⏱ wait 109s · review 65s · post 17s → /r/y.md
[1450] review fix/g@cfd6309: clean (0 important, 1 nits) · ⏱ review 173s · post 8s → /r/w.md
[1500] ci uat@f647c6c: 🧪 2 important, 0 pre-existing · ⏱ review 10s · post 18s → /r/z.md
[1600] uat: ✅ ready to merge @f647c6c (review clean, CI green)
[1700] holding: resource guard (avail 5.7 GiB, load 13.14, heavy 3)
[1800] inbox: error connecting to api.github.com
[1900] inbox: error connecting to api.github.com
[2000] fix/c: comment: HTTP 502
[2100] fix/d: review failed: bad json: EOF (raw kept at /r/out)
[2200] fix/e: usage limit, paused until 9999
not a log line
";

    #[test] fn counts_each_kind_of_result_since_the_cutoff() {
        let r = summarize(LOG, 0);
        // A partial review can also have important findings: it counts in both.
        assert_eq!((r.reviews, r.clean, r.important, r.partial), (4, 2, 1, 2));
        assert_eq!((r.ci, r.ready, r.holds, r.usage_limits), (1, 1, 1, 1));
        assert_eq!(r.failed, vec!["fix/d: review failed: bad json: EOF (raw kept at /r/out)".to_string()]);
        assert_eq!(r.refused, vec!["fix/a: Bash: for f in a b; do grep -n x $f; done".to_string()]);
        assert_eq!(r.errors, vec![("inbox: error connecting to api.github.com".to_string(), 2),
                                  ("fix/c: comment: HTTP 502".to_string(), 1)]);
        assert_eq!(r.review_secs, vec![251, 221, 65, 173], "with and without a queue wait");
        assert_eq!(r.wait_secs, vec![782, 200, 109]);
        let late = summarize(LOG, 1350);
        assert_eq!((late.reviews, late.important, late.partial, late.refused.len()), (2, 0, 0, 0), "older lines are left out");
    }
    #[test] fn renders_counts_medians_and_lists() {
        let out = render(&summarize(LOG, 0), 24);
        for want in ["revaiw report · last 24h", "reviews 4: 2 clean · 1 with important findings · 2 ⚠️ partial",
                     "ci diagnoses 1 · ✅ ready to merge 1", "review time median 221s (max 251s) · queue wait median 200s",
                     "usage-limit pauses 1 · resource-guard holds 1", "refused commands (1)", "fix/a: Bash: for f in",
                     "failed (1)", "errors (2 kinds)", "2× inbox: error connecting to api.github.com"] {
            assert!(out.contains(want), "missing {want:?} in:\n{out}");
        }
    }
    #[test] fn a_quiet_log_says_so() {
        let out = render(&summarize("", 0), 6);
        assert!(out.contains("no reviews in the last 6h"), "{out}");
        assert!(!out.contains("refused") && !out.contains("errors"), "{out}");
        let one = render(&summarize("[1] inbox: error connecting to api.github.com\n", 0), 6);
        assert!(one.contains("errors (1 kind):"), "{one}");
    }
    #[test] fn hours_default_to_24_and_must_be_a_positive_number() {
        assert_eq!(hours_arg(&[]), Ok(24));
        assert_eq!(hours_arg(&["6".into()]), Ok(6));
        assert!(hours_arg(&["0".into()]).is_err());
        assert!(hours_arg(&["x".into()]).is_err());
    }
}
