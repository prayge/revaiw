use crate::config::{path_matches, Config};
use crate::filter::is_generated;

pub struct Route {
    pub model: String,
    pub effort: &'static str,
    /// The `/code-review` level the engine runs at: low on the small model, medium on the others (never high, for
    /// usage).
    pub level: &'static str,
    pub reason: String,
}

/// Small model by default; the medium one for large or risky ranges; the large one for very large ranges or ones
/// on `routing.opus_paths`. Decided from git alone, no model call. A follow-up (fixing open important findings) goes to the
/// medium model for `opus_paths`: the large model on every fix push found edges of edges (spec C4). The size rules
/// still apply.
pub fn route(c: &Config, files: &[(u32, u32, String)], messages: &str, follow_up: bool) -> Route {
    let real: Vec<_> = files.iter().filter(|(_, _, p)| !is_generated(c, p)).collect();
    let lines: u32 = real.iter().map(|(a, d, _)| a + d).sum();
    let large = |reason: String| Route { model: c.models.large.clone(), effort: "high", level: "medium", reason };
    let medium = |reason: String| Route { model: c.models.medium.clone(), effort: "medium", level: "medium", reason };
    let touching = |pats: &[String]| real.iter().map(|(_, _, p)| p)
        .find(|p| pats.iter().any(|pat| path_matches(pat, p))).cloned();
    if lines > 1500 { return large(format!("{lines} lines")); }
    if real.len() > 40 { return large(format!("{} files", real.len())); }
    if let Some(p) = touching(&c.routing.opus_paths) {
        return if follow_up { medium(format!("follow-up, touches {p}")) } else { large(format!("touches {p}")) };
    }
    if lines > 400 { return medium(format!("{lines} lines")); }
    if real.len() > 15 { return medium(format!("{} files", real.len())); }
    if let Some(p) = touching(&c.routing.sonnet_paths) { return medium(format!("touches {p}")); }
    let lower = messages.to_lowercase();
    let words: Vec<&str> = lower.split(|ch: char| !ch.is_alphanumeric()).collect();
    if let Some(w) = c.routing.risk_words.iter().find(|w| words.contains(&w.to_lowercase().as_str())) {
        return medium(format!("commit mentions '{w}'"));
    }
    Route { model: c.models.small.clone(), effort: "high", level: "low",
            reason: format!("small ({lines} lines), no risk paths") }
}

/// The tier names `now` and `@review --model` accept.
pub fn tier_ok(t: &str) -> bool { matches!(t, "haiku" | "sonnet" | "opus") }

/// A pinned tier at its routed effort, whatever the range's size.
pub fn pinned(c: &Config, tier: &str) -> Option<Route> {
    let (model, effort, level) = match tier {
        "haiku" => (&c.models.small, "high", "low"),
        "sonnet" => (&c.models.medium, "medium", "medium"),
        "opus" => (&c.models.large, "high", "medium"),
        _ => return None,
    };
    Some(Route { model: model.clone(), effort, level, reason: format!("pinned to {tier}") })
}

/// `now [--model <tier>] [branch...]`.
pub fn now_args(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut it = args.iter();
    let mut tier = None;
    let mut branches = vec![];
    while let Some(a) = it.next() {
        let t = match a.strip_prefix("--model=") {
            Some(t) => t.to_string(),
            None if a == "--model" => it.next().ok_or("--model needs haiku, sonnet or opus")?.clone(),
            None if a.starts_with('-') => return Err(format!("unknown flag {a}: only --model <haiku|sonnet|opus>")),
            None => { branches.push(a.clone()); continue; }
        };
        if !tier_ok(&t) { return Err(format!("unknown model {t}: use haiku, sonnet or opus")); }
        tier = Some(t);
    }
    Ok((tier, branches))
}

/// The `now-requests` lines for one CLI call, appended in a single write: each branch, then a tab and the pinned
/// tier if any (branch names have no tabs).
pub fn now_body(branches: &[String], tier: Option<&str>) -> String {
    branches.iter().map(|b| match tier { Some(t) => format!("{b}\t{t}\n"), None => format!("{b}\n") }).collect()
}

/// A request line: the branch, and its tier if it names a known one (`Err` carries an unknown one).
pub fn parse_now_line(line: &str) -> (String, Result<Option<String>, String>) {
    let mut parts = line.trim().splitn(2, '\t');
    let branch = parts.next().unwrap_or("").trim().to_string();
    let tier = match parts.next().map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(None),
        Some(t) if tier_ok(t) => Ok(Some(t.to_string())),
        Some(t) => Err(t.to_string()),
    };
    (branch, tier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{resolve, Config, HAIKU, OPUS, SONNET};
    fn c() -> Config { resolve(Config::default(), "main") }
    fn f(a: u32, p: &str) -> (u32, u32, String) { (a, 0, p.to_string()) }
    #[test] fn each_tier_runs_code_review_at_its_level() {
        assert_eq!(route(&c(), &[f(30, "apps/a.tsx")], "", false).level, "low");
        assert_eq!(route(&c(), &[f(401, "a.ts")], "", false).level, "medium");
        assert_eq!(route(&c(), &[f(1501, "a.ts")], "", false).level, "medium", "Opus runs at medium, for usage");
        assert_eq!(pinned(&c(), "sonnet").unwrap().level, "medium");
        assert_eq!(pinned(&c(), "haiku").unwrap().level, "low");
        assert_eq!(pinned(&c(), "opus").unwrap().level, "medium");
    }
    #[test] fn a_pin_names_a_tier() {
        let r = pinned(&c(), "sonnet").unwrap();
        assert_eq!((r.model.as_str(), r.effort), (SONNET, "medium"));
        assert!(r.reason.contains("pinned"));
        assert_eq!(pinned(&c(), "opus").unwrap().model, OPUS);
        assert_eq!(pinned(&c(), "haiku").unwrap().model, HAIKU);
        assert!(pinned(&c(), "gpt").is_none());
    }
    #[test] fn now_takes_a_model_and_several_branches() {
        let a = |v: &[&str]| now_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&["--model", "sonnet", "feat/a", "feat/b"]),
                   Ok((Some("sonnet".to_string()), vec!["feat/a".to_string(), "feat/b".into()])));
        assert_eq!(a(&["feat/a"]), Ok((None, vec!["feat/a".to_string()])));
        assert_eq!(a(&[]), Ok((None, vec![])));
        assert!(a(&["--model", "gpt", "feat/a"]).is_err());
        assert!(a(&["--model"]).is_err());
    }
    #[test] fn now_request_lines_round_trip() {
        assert_eq!(now_body(&["feat/a".into(), "feat/b".into()], Some("sonnet")), "feat/a\tsonnet\nfeat/b\tsonnet\n");
        assert_eq!(parse_now_line("feat/x\tsonnet"), ("feat/x".to_string(), Ok(Some("sonnet".to_string()))));
        assert_eq!(parse_now_line(" feat/x \n"), ("feat/x".to_string(), Ok(None)));
        assert_eq!(parse_now_line("feat/x\tsonet"), ("feat/x".to_string(), Err("sonet".to_string())));
    }
    #[test] fn now_takes_model_equals_and_rejects_other_flags() {
        let a = |v: &[&str]| now_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&["--model=opus", "feat/a"]), Ok((Some("opus".to_string()), vec!["feat/a".to_string()])));
        assert!(a(&["--force", "feat/a"]).is_err());
        assert!(a(&["--model=gpt"]).is_err());
    }
    #[test] fn small_goes_haiku() {
        let r = route(&c(), &[f(30, "apps/platform/src/a.tsx")], "tweak copy", false);
        assert_eq!((r.model.as_str(), r.effort), (HAIKU, "high"));
    }
    #[test] fn huge_or_high_risk_goes_opus() {
        let r = route(&c(), &[f(1501, "a.ts")], "", false);
        assert_eq!((r.model.as_str(), r.effort), (OPUS, "high"));
        let many: Vec<_> = (0..41).map(|i| f(1, &format!("f{i}.ts"))).collect();
        assert_eq!(route(&c(), &many, "", false).model, OPUS);
        for p in ["packages/database/prisma/schema.prisma", "packages/database/prisma/migrations/1/migration.sql",
                  "apps/backend/src/auth/session.ts", "apps/backend/src/services/billing/x.ts",
                  "apps/backend/src/stripe/x.ts", "packages/database/sql/rls_policies.sql"] {
            assert_eq!(route(&c(), &[f(1, p)], "", false).model, OPUS, "{p}");
        }
        assert_eq!(route(&c(), &[f(1500, "a.ts")], "", false).model, SONNET);
    }
    #[test] fn a_follow_up_on_a_risky_path_runs_on_the_medium_model() {
        let r = route(&c(), &[f(10, "apps/api/billing/x.ts")], "", true);
        assert_eq!(r.model, SONNET, "{}", r.reason);
        assert!(r.reason.contains("follow-up"), "{}", r.reason);
        assert_eq!(route(&c(), &[f(10, "apps/api/billing/x.ts")], "", false).model, OPUS, "a first review still gets Opus");
        assert_eq!(route(&c(), &[f(1501, "a.ts")], "", true).model, OPUS, "a huge follow-up is a new review");
    }
    #[test] fn big_goes_sonnet() {
        assert_eq!(route(&c(), &[f(401, "a.ts")], "", false).model, SONNET);
        let many: Vec<_> = (0..16).map(|i| f(1, &format!("f{i}.ts"))).collect();
        assert_eq!(route(&c(), &many, "", false).model, SONNET);
    }
    #[test] fn generated_lines_do_not_count() {
        assert_eq!(route(&c(), &[f(5000, "graphify-out/graph.json"), f(3, "a.ts")], "", false).model, HAIKU);
    }
    #[test] fn risk_paths() {
        for p in ["packages/database/src/validations/partner/x.ts", "apps/backend/src/connectors/meta.ts",
                  "apps/backend/src/integrations/google.ts", ".github/workflows/ci.yml", "infra/scripts/x.mts"] {
            assert_eq!(route(&c(), &[f(1, p)], "", false).model, SONNET, "{p}");
        }
        assert_eq!(route(&c(), &[f(1, "apps/landing/src/urls.ts")], "", false).model, HAIKU);
    }
    #[test] fn risk_words_in_commits() {
        let r = route(&c(), &[f(1, "a.ts")], "fix: Migration ordering", false);
        assert_eq!(r.model, SONNET);
        assert!(r.reason.contains("migration"));
        assert_eq!(route(&c(), &[f(1, "a.ts")], "authority label", false).model, HAIKU);
    }
    #[test] fn a_configured_risk_word_matches_whatever_its_case() {
        let mut cfg = c();
        cfg.routing.risk_words = vec!["Payment".into()];
        assert_eq!(route(&cfg, &[f(1, "a.ts")], "fix payment retry", false).model, cfg.models.medium);
    }
    #[test] fn config_sets_risk_paths_words_and_models() {
        let mut cfg = c();
        cfg.routing.sonnet_paths.push("/graph/public/".into());
        cfg.routing.risk_words = vec!["odds".into()];
        cfg.models.medium = "m".into();
        assert_eq!(route(&cfg, &[f(1, "packages/database/src/graph/public/operations.ts")], "", false).model, "m");
        assert_eq!(route(&cfg, &[f(1, "a.ts")], "fix odds rounding", false).model, "m");
        assert_eq!(route(&cfg, &[f(1, "a.ts")], "migration", false).model, HAIKU, "the list replaces the defaults");
    }
}
