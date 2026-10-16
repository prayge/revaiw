use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Finding {
    pub severity: String, // important | nit | pre_existing
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub end_line: Option<u32>,
    pub summary: String,
    pub why: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub suggested_patch: Option<String>,
    #[serde(default)]
    pub suggestion: Option<String>,
    /// CI diagnoses only: the model confirmed the failure doesn't involve anything this branch changed.
    #[serde(default)]
    pub on_base: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Walk {
    pub file: String,
    pub change: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ReviewOutput {
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub resolved: Vec<String>,
    #[serde(default)]
    pub still_open: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub walkthrough: Vec<Walk>,
    /// Set by the formatter: the engine's answer says it did not finish (could not read, ran out, gave up).
    #[serde(default)]
    pub incomplete: bool,
    /// Set by the daemon, never the model: the tool calls `/code-review` was refused (`tool: command`). Each is
    /// something it could not read or check, so non-empty means the review is partial, never "clean".
    #[serde(default)]
    pub refused: Vec<String>,
}

impl ReviewOutput {
    /// Partial: the engine was refused a read, or the formatter judged its answer incomplete. Never "clean".
    pub fn partial(&self) -> bool { self.incomplete || !self.refused.is_empty() }

    /// What made it partial, in words, e.g. "did not complete the review; was refused 2 reads".
    pub fn gaps(&self) -> String {
        let n = self.refused.len();
        let mut parts = vec![];
        if self.incomplete { parts.push("did not complete the review".to_string()); }
        if n > 0 { parts.push(format!("was refused {n} read{}", if n == 1 { "" } else { "s" })); }
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn a_review_is_partial_when_refused_or_incomplete_and_says_which() {
        let mut o = ReviewOutput::default();
        assert!(!o.partial());
        o.incomplete = true;
        assert!(o.partial());
        assert_eq!(o.gaps(), "did not complete the review");
        o.refused = vec!["Bash: x".into()];
        assert_eq!(o.gaps(), "did not complete the review; was refused 1 read");
        o.incomplete = false;
        assert_eq!(o.gaps(), "was refused 1 read");
    }
}
