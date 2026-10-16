//! `@review` mentions and reactions: what a comment asks for, who may ask, and what has been seen. The polling
//! itself (gh calls) is in `inbox.rs`; this module is pure, so every rule here is tested without GitHub.
//! @see README.md
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Review { model: Option<String>, text: String },
    /// Empty `ids`: every open important finding.
    Fix { ids: Vec<String> },
}

fn is_finding_id(w: &str) -> bool { w.len() == 12 && w.chars().all(|c| c.is_ascii_hexdigit()) }

/// The command in a PR comment: the first line that starts with `trigger` as a whole word (case-insensitive).
/// The rest of that line, plus any lines after it, is the request. revaiw's own comments (they carry
/// `render::MARKER`) never count: they are posted under the user's own login, so the author cannot tell them apart.
pub fn parse(body: &str, trigger: &str) -> Option<Cmd> {
    if body.contains(crate::render::MARKER) { return None; }
    let t = trigger.to_lowercase();
    let lines: Vec<&str> = body.lines().collect();
    let (i, rest) = lines.iter().enumerate().find_map(|(i, l)| {
        let l = l.trim_start();
        let head = l.get(..t.len())?;
        let after = &l[t.len()..];
        (head.to_lowercase() == t && after.chars().next().is_none_or(char::is_whitespace)).then(|| (i, after.trim()))
    })?;
    let mut words = rest.split_whitespace();
    match words.next() {
        Some("fix") => return Some(Cmd::Fix { ids: words.filter(|w| is_finding_id(w)).map(str::to_string).collect() }),
        Some("--model") => {
            let tier = words.next().unwrap_or("");
            if crate::route::tier_ok(tier) {
                let text = rest.trim_start_matches("--model").trim_start().strip_prefix(tier).unwrap_or("").trim();
                return Some(Cmd::Review { model: Some(tier.into()), text: join(text, &lines[i + 1..]) });
            }
        }
        _ => {}
    }
    Some(Cmd::Review { model: None, text: join(rest, &lines[i + 1..]) })
}

fn join(first: &str, more: &[&str]) -> String {
    std::iter::once(first).chain(more.iter().copied()).collect::<Vec<_>>().join("\n").trim().to_string()
}

pub fn can_trigger(permission: &str) -> bool { matches!(permission, "admin" | "maintain" | "write") }

/// Collaborator permissions by login, cached for an hour. A failed lookup denies (fail closed) and is not cached.
#[derive(Default)]
pub struct Perms { cache: HashMap<String, (bool, u64)> }

impl Perms {
    /// Err when the lookup itself failed (not cached, so the next request asks again): never a grant.
    pub fn allowed(&mut self, login: &str, now: u64, lookup: &dyn Fn(&str) -> Result<String, String>) -> Result<bool, String> {
        if let Some((ok, at)) = self.cache.get(login) {
            if now.saturating_sub(*at) < 3600 { return Ok(*ok); }
        }
        let ok = can_trigger(lookup(login)?.trim());
        self.cache.insert(login.into(), (ok, now));
        Ok(ok)
    }
}

/// What the inbox has handled, in `<state>/mentions.json`: a restart never re-runs a comment or a reaction.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    /// ISO time of the newest comment seen; the next poll asks for comments since then.
    pub since: String,
    pub seen: BTreeSet<u64>,
    /// Review comment id → relevant reactions (👍 ❤️ 👎 😕) already handled.
    pub reactions: BTreeMap<u64, u32>,
}

impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) {
        crate::status::write_atomic(path, &serde_json::to_vec_pretty(self).unwrap());
    }
    /// Marks a comment handled. Ids grow over time, so keeping the newest 1000 bounds the file. GitHub's `since`
    /// is inclusive, so the ids are what stop the boundary comment from running twice.
    pub fn remember(&mut self, id: u64, created_at: &str) {
        self.seen.insert(id);
        while self.seen.len() > 1000 { self.seen.pop_first(); }
        if created_at > self.since.as_str() { self.since = created_at.to_string(); }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Act { Fix, Dismiss }

pub fn act(content: &str) -> Option<Act> {
    match content { "+1" | "heart" => Some(Act::Fix), "-1" | "confused" => Some(Act::Dismiss), _ => None }
}

/// The next poll interval: doubled (up to 15 min) on a GitHub rate limit, back to `base` after a success, unchanged
/// after any other error.
pub fn next_interval(current: u64, base: u64, err: Option<&str>) -> u64 {
    match err {
        None => base,
        Some(e) if e.contains("403") || e.contains("429") || e.to_lowercase().contains("rate limit") => (current * 2).min(900),
        Some(_) => current,
    }
}

/// An epoch as `YYYY-MM-DDTHH:MM:SSZ` (UTC), for GitHub's `since`. Civil-from-days (Howard Hinnant's algorithm).
pub fn iso(epoch: u64) -> String {
    let (days, rem) = ((epoch / 86_400) as i64, epoch % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test] fn a_bare_mention_is_a_review() {
        assert_eq!(parse("@review", "@review"), Some(Cmd::Review { model: None, text: String::new() }));
        assert_eq!(parse("thanks!\n@Review is the retry safe?\nmore context", "@review"),
                   Some(Cmd::Review { model: None, text: "is the retry safe?\nmore context".into() }));
    }
    #[test] fn a_model_can_be_pinned() {
        assert_eq!(parse("@review --model opus check auth", "@review"),
                   Some(Cmd::Review { model: Some("opus".into()), text: "check auth".into() }));
        assert_eq!(parse("@review --model  opus check auth", "@review"),
                   Some(Cmd::Review { model: Some("opus".into()), text: "check auth".into() }), "repeated whitespace");
        assert_eq!(parse("@review --model gpt x", "@review"),
                   Some(Cmd::Review { model: None, text: "--model gpt x".into() }), "unknown tier stays text");
    }
    #[test] fn fix_with_or_without_ids() {
        assert_eq!(parse("@review fix", "@review"), Some(Cmd::Fix { ids: vec![] }));
        assert_eq!(parse("@review fix 0123456789ab deadbeef0000 nope", "@review"),
                   Some(Cmd::Fix { ids: vec!["0123456789ab".into(), "deadbeef0000".into()] }));
        assert_eq!(parse("@review fixing the bug?", "@review"),
                   Some(Cmd::Review { model: None, text: "fixing the bug?".into() }));
    }
    #[test] fn the_mention_must_start_a_line_and_end_a_word() {
        assert_eq!(parse("cc @review please", "@review"), None);
        assert_eq!(parse("@reviewer look", "@review"), None);
        assert_eq!(parse("`@review` is the trigger", "@review"), None);
        assert_eq!(parse("@bot fix", "@bot"), Some(Cmd::Fix { ids: vec![] }));
    }
    #[test] fn revaiw_comments_never_trigger() {
        let own = format!("{}\n## 🐇 Review\nReply `@review fix` to apply.\n@review fix", crate::render::MARKER);
        assert_eq!(parse(&own, "@review"), None);
    }
    #[test] fn only_writers_trigger() {
        for p in ["admin", "maintain", "write"] { assert!(can_trigger(p)); }
        for p in ["triage", "read", "none", ""] { assert!(!can_trigger(p)); }
    }
    #[test] fn permissions_are_cached_for_an_hour() {
        let calls = Cell::new(0);
        let lookup = |_: &str| { calls.set(calls.get() + 1); Ok("write".to_string()) };
        let mut p = Perms::default();
        assert_eq!(p.allowed("kim", 0, &lookup), Ok(true));
        assert_eq!(p.allowed("kim", 3599, &lookup), Ok(true));
        assert_eq!(calls.get(), 1);
        assert_eq!(p.allowed("kim", 3600, &lookup), Ok(true));
        assert_eq!(calls.get(), 2);
        assert_eq!(p.allowed("eve", 0, &|_| Ok("read".to_string())), Ok(false));
    }
    #[test] fn a_failed_lookup_denies_and_is_not_cached() {
        let mut p = Perms::default();
        assert_eq!(p.allowed("kim", 0, &|_| Err("HTTP 502".to_string())), Err("HTTP 502".to_string()));
        assert_eq!(p.allowed("kim", 1, &|_| Ok("write".to_string())), Ok(true), "a later success is not blocked by the failure");
    }
    #[test] fn state_remembers_comments_and_moves_the_cursor_forward_only() {
        let mut s = State::default();
        s.remember(5, "2026-10-15T10:00:00Z");
        s.remember(4, "2026-10-15T09:00:00Z");
        assert_eq!(s.since, "2026-10-15T10:00:00Z");
        assert!(s.seen.contains(&4) && s.seen.contains(&5));
        for i in 0..1100 { s.remember(100 + i, "2026-10-15T11:00:00Z"); }
        assert!(s.seen.len() <= 1000 && !s.seen.contains(&4), "bounded: oldest ids dropped");
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("mentions.json");
        s.save(&path);
        assert_eq!(State::load(&path).since, "2026-10-15T11:00:00Z");
    }
    #[test] fn reactions_map_to_actions() {
        assert_eq!(act("+1"), Some(Act::Fix));
        assert_eq!(act("heart"), Some(Act::Fix));
        assert_eq!(act("-1"), Some(Act::Dismiss));
        assert_eq!(act("confused"), Some(Act::Dismiss));
        assert_eq!(act("rocket"), None);
    }
    #[test] fn rate_limits_back_off_and_success_resets() {
        assert_eq!(next_interval(60, 60, Some("HTTP 403: API rate limit exceeded")), 120);
        assert_eq!(next_interval(600, 60, Some("HTTP 429")), 900);
        assert_eq!(next_interval(900, 60, Some("rate limit")), 900);
        assert_eq!(next_interval(240, 60, Some("HTTP 502")), 240, "other errors keep the interval");
        assert_eq!(next_interval(240, 60, None), 60);
    }
    #[test] fn iso_formats_utc() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_792_065_600), "2026-10-15T12:00:00Z");
    }
}
