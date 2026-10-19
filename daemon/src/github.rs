use crate::model::Finding;
use crate::render::inline_body;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

pub struct Pr {
    pub number: u64,
    pub base: String,
}

pub fn posting_enabled() -> bool {
    std::env::var("REVAIW_NO_POST").map_or(true, |v| v != "1")
}

/// Every gh call is bounded: one hung network call would otherwise freeze the single-threaded daemon.
fn gh_command(args: &[&str]) -> Command {
    let mut c = Command::new("timeout");
    c.args(["-k", "5", "60", "gh"]).args(args);
    c
}

fn gh(args: &[&str], input: Option<&str>) -> Result<String, String> {
    let mut c = gh_command(args);
    c.stdout(Stdio::piped()).stderr(Stdio::piped())
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().map_err(|e| format!("gh: {e}"))?;
    if let (Some(i), Some(mut s)) = (input, child.stdin.take()) { let _ = s.write_all(i.as_bytes()); }
    let o = child.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()) }
    else { Err(String::from_utf8_lossy(&o.stderr).chars().take(300).collect()) }
}

pub fn repo_slug() -> Option<String> {
    try_repo_slug().ok()
}

/// `repo_slug` with gh's reason when it fails.
pub fn try_repo_slug() -> Result<String, String> {
    gh(&["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"], None)
}

pub fn pr_for(branch: &str) -> Option<Pr> {
    let out = gh(&["pr", "list", "--head", branch, "--state", "open", "--json", "number,baseRefName", "--limit", "1"], None).ok()?;
    let v: Value = serde_json::from_str(&out).ok()?;
    let p = v.get(0)?;
    Some(Pr { number: p["number"].as_u64()?, base: p["baseRefName"].as_str()?.to_string() })
}

/// The base branch of `branch`'s open PR: Ok(None) when it has none, Err when gh or its output failed —
/// which `pr_for` folds into None, and a CI decision must not.
pub fn open_pr_base(branch: &str) -> Result<Option<String>, ()> {
    let out = gh(&["pr", "list", "--head", branch, "--state", "open", "--json", "baseRefName", "--limit", "1"], None)
        .map_err(|_| ())?;
    let v: Value = serde_json::from_str(&out).map_err(|_| ())?;
    let list = v.as_array().ok_or(())?;
    Ok(list.first().and_then(|p| p["baseRefName"].as_str()).map(str::to_string))
}

/// A new comment on every push; earlier ones stay as history.
pub fn post_comment(slug: &str, pr: &Pr, body: &str) -> Result<(), String> {
    let payload = json!({ "body": body }).to_string();
    gh(&["api", "-X", "POST", &format!("repos/{slug}/issues/{}/comments", pr.number), "--input", "-"], Some(&payload))
        .map(|_| ())
}

#[derive(Debug, PartialEq, Clone)]
pub struct IssueComment { pub id: u64, pub pr: u64, pub login: String, pub body: String, pub created_at: String }

/// PR conversation comments (issue comments whose page is a pull request), oldest first.
pub fn parse_issue_comments(json: &str) -> Vec<IssueComment> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut out: Vec<IssueComment> = v.as_array().into_iter().flatten().filter_map(|c| {
        let url = c["html_url"].as_str()?;
        if !url.contains("/pull/") { return None; }
        Some(IssueComment {
            id: c["id"].as_u64()?,
            pr: c["issue_url"].as_str()?.rsplit('/').next()?.parse().ok()?,
            login: c["user"]["login"].as_str()?.to_string(),
            body: c["body"].as_str().unwrap_or_default().to_string(),
            created_at: c["created_at"].as_str()?.to_string(),
        })
    }).collect();
    out.sort_by(|a, b| (a.created_at.as_str(), a.id).cmp(&(b.created_at.as_str(), b.id)));
    out
}

/// One call per poll for every new comment in the repo. `since` filters by update time, so an edited comment
/// comes back too; `mention::State::seen` stops it running twice.
pub fn issue_comments_since(slug: &str, since: &str) -> Result<Vec<IssueComment>, String> {
    gh(&["api", &format!("repos/{slug}/issues/comments?since={since}&per_page=100&sort=created&direction=asc")], None)
        .map(|o| parse_issue_comments(&o))
}

#[derive(Debug, PartialEq, Clone)]
pub struct ReviewComment { pub id: u64, pub relevant: u32 }

/// Inline review comments with their count of the reactions revaiw acts on (👍 ❤️ 👎 😕), from GitHub's rollup.
pub fn parse_review_comments(json: &str) -> Vec<ReviewComment> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    v.as_array().into_iter().flatten().filter_map(|c| {
        let r = &c["reactions"];
        let n = ["+1", "-1", "heart", "confused"].iter().map(|k| r[*k].as_u64().unwrap_or(0) as u32).sum();
        Some(ReviewComment { id: c["id"].as_u64()?, relevant: n })
    }).collect()
}

pub fn review_comments(slug: &str, pr: u64) -> Result<Vec<ReviewComment>, String> {
    gh(&["api", &format!("repos/{slug}/pulls/{pr}/comments?per_page=100")], None).map(|o| parse_review_comments(&o))
}

#[derive(Debug, PartialEq, Clone)]
pub struct Reaction { pub id: u64, pub login: String, pub content: String }

pub fn parse_reactions(json: &str) -> Vec<Reaction> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut out: Vec<Reaction> = v.as_array().into_iter().flatten().filter_map(|r| Some(Reaction {
        id: r["id"].as_u64()?, login: r["user"]["login"].as_str()?.to_string(), content: r["content"].as_str()?.to_string(),
    })).collect();
    out.sort_by_key(|r| r.id);
    out
}

pub fn reactions(slug: &str, comment_id: u64) -> Result<Vec<Reaction>, String> {
    gh(&["api", &format!("repos/{slug}/pulls/comments/{comment_id}/reactions?per_page=100")], None)
        .map(|o| parse_reactions(&o))
}

/// 👀 (`eyes`) on a mention once it is queued.
pub fn add_reaction(slug: &str, issue_comment_id: u64, content: &str) {
    let _ = gh(&["api", "-X", "POST", &format!("repos/{slug}/issues/comments/{issue_comment_id}/reactions"),
                 "-f", &format!("content={content}")], None);
}

/// `admin`, `maintain`, `write`, `triage`, `read` or `none`. A non-collaborator is a 404, so an `Err`.
pub fn permission(slug: &str, login: &str) -> Result<String, String> {
    gh(&["api", &format!("repos/{slug}/collaborators/{login}/permission"), "--jq", ".permission"], None)
}

pub fn post_issue_comment(slug: &str, number: u64, body: &str) -> Result<(), String> {
    post_comment(slug, &Pr { number, base: String::new() }, body)
}

#[derive(Clone, Debug)]
pub struct CiRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
}

pub enum CiState {
    Pending,
    Passed,
    /// Every run skipped itself (a draft PR: the workflows gate on `draft == false`). Nothing ran, so not green.
    Skipped,
    Failed(Vec<CiRun>),
}

/// Pending until every run for the commit has completed (and at least one exists). Green needs at least one run
/// that actually ran: all-skipped (a draft) is `Skipped`.
pub fn ci_state(runs: &[CiRun]) -> CiState {
    if runs.is_empty() || runs.iter().any(|r| r.status != "completed") { return CiState::Pending; }
    let failed: Vec<CiRun> = runs.iter()
        .filter(|r| matches!(r.conclusion.as_str(), "failure" | "timed_out" | "startup_failure"))
        .cloned().collect();
    if !failed.is_empty() { return CiState::Failed(failed); }
    if runs.iter().all(|r| r.conclusion == "skipped") { CiState::Skipped } else { CiState::Passed }
}

pub fn ci_runs(sha: &str) -> Option<Vec<CiRun>> {
    let out = gh(&["run", "list", "--commit", sha, "--json", "databaseId,status,conclusion,workflowName", "--limit", "50"], None).ok()?;
    let v: Vec<Value> = serde_json::from_str(&out).ok()?;
    Some(v.iter().map(|r| CiRun {
        id: r["databaseId"].as_u64().unwrap_or(0),
        name: r["workflowName"].as_str().unwrap_or("").into(),
        status: r["status"].as_str().unwrap_or("").into(),
        conclusion: r["conclusion"].as_str().unwrap_or("").into(),
    }).collect())
}

/// The last `lines` lines of a log, capped at `max` bytes (cut on a char boundary).
pub fn log_tail(log: &str, lines: usize, max: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    let mut t = all[all.len().saturating_sub(lines)..].join("\n");
    if t.len() > max {
        let mut cut = t.len() - max;
        while !t.is_char_boundary(cut) { cut += 1; }
        t = t[cut..].to_string();
    }
    t
}

/// None when gh fails: that is no answer, not an empty log (which reads as an Actions outage, spec C6).
pub fn failed_log(run_id: u64) -> Option<String> {
    gh(&["run", "view", &run_id.to_string(), "--log-failed"], None).ok().map(|l| log_tail(&l, 150, 12_000))
}

/// Commit status for the PR's checks list, posted only when a review completes, and always "success" with the
/// counts. A daemon that is off, a timeout or a usage limit is "not reviewed", which shows as no status at all,
/// never as pending/error/failure: statuses never expire, and not reviewing is not a fault.
/// Findings never fail the commit. A partial review posts nothing: no status already means "not reviewed".
pub fn status_text(important: usize, nits: usize, refused: usize) -> Option<(&'static str, String)> {
    if refused > 0 { return None; }
    if important + nits == 0 { return Some(("success", "No open findings".into())); }
    Some(("success", format!("{important} important · {nits} nit{}", if nits == 1 { "" } else { "s" })))
}

pub fn set_status(slug: &str, sha: &str, state: &str, description: &str) {
    let payload = json!({ "state": state, "context": "revaiw", "description": description }).to_string();
    let _ = gh(&["api", "-X", "POST", &format!("repos/{slug}/statuses/{sha}"), "--input", "-"], Some(&payload));
}

/// The inline review for important findings that sit on added lines. None when nothing qualifies.
pub fn review_payload(head: &str, items: &[(String, Finding)], added: &HashMap<String, HashSet<u32>>)
    -> Option<(Value, Vec<String>)> {
    let mut comments = vec![];
    let mut ids = vec![];
    for (id, f) in items.iter().filter(|(_, f)| f.severity == "important") {
        let Some(lines) = added.get(&f.file) else { continue };
        let end = f.end_line.unwrap_or(f.line).max(f.line);
        if !lines.contains(&end) { continue; }
        // A suggestion replaces line..end_line; without start_line GitHub would replace only `end`.
        let ranged = end > f.line && lines.contains(&f.line);
        let mut c = json!({ "path": f.file, "line": end, "side": "RIGHT", "body": inline_body(f, end == f.line || ranged) });
        if ranged {
            c["start_line"] = json!(f.line);
            c["start_side"] = json!("RIGHT");
        }
        comments.push(c);
        ids.push(id.clone());
    }
    (!comments.is_empty()).then(|| (json!({ "commit_id": head, "event": "COMMENT", "body": "", "comments": comments }), ids))
}

/// The review's comments come back in the order they were sent, so ids pair by position.
pub fn pair_ids(payload: &Value, posted: &[Value]) -> Vec<Option<u64>> {
    let n = payload["comments"].as_array().map_or(0, |a| a.len());
    (0..n).map(|i| posted.get(i).and_then(|p| p["id"].as_u64())).collect()
}

/// Posts the review and returns one id per payload comment. Once the POST has succeeded, a failed id
/// lookup still marks every comment as posted (id 0) so the next push never posts them again.
pub fn post_review(slug: &str, pr: &Pr, payload: &Value) -> Result<Vec<u64>, String> {
    let out = gh(&["api", "-X", "POST", &format!("repos/{slug}/pulls/{}/reviews", pr.number), "--input", "-"],
                 Some(&payload.to_string()))?;
    let n = payload["comments"].as_array().map_or(0, |a| a.len());
    let ids = serde_json::from_str::<Value>(&out).ok().and_then(|v| v["id"].as_u64())
        .and_then(|rid| gh(&["api", &format!("repos/{slug}/pulls/{}/reviews/{rid}/comments?per_page=100", pr.number)], None).ok())
        .and_then(|o| serde_json::from_str::<Vec<Value>>(&o).ok())
        .map(|posted| pair_ids(payload, &posted).into_iter().map(|i| i.unwrap_or(0)).collect())
        .unwrap_or_else(|| vec![0; n]);
    Ok(ids)
}

pub struct Thread {
    pub id: String,
    /// The database id of the thread's first comment: what the ledger records as `comment_id`.
    pub first: Option<u64>,
    pub resolved: bool,
}

pub struct PrComment {
    pub id: String,
    pub body: String,
    pub hidden: bool,
}

pub fn threads_to_resolve(threads: &[Thread], closed: &HashSet<u64>) -> Vec<String> {
    threads.iter().filter(|t| !t.resolved && t.first.is_some_and(|f| closed.contains(&f))).map(|t| t.id.clone()).collect()
}

/// Whether a revaiw comment is a CI diagnosis. The kind is the comment's own heading, the line right after
/// the marker (`render::summary`), never text quoted further down.
pub fn is_ci_comment(body: &str) -> bool {
    body.lines().nth(1).is_some_and(|h| h.starts_with("## 🧪 CI failures"))
}

/// Whether a revaiw comment is a review summary: its own heading, like `is_ci_comment`. Fix comments (`## 🔧 Fix`)
/// and replies (no heading) are neither, so they never stand in for, or hide, the review.
pub fn is_review_comment(body: &str) -> bool {
    body.lines().nth(1).is_some_and(|h| h.starts_with("## 🐇 Review"))
}

/// The older review summaries and the older CI diagnoses, oldest first: the newest of each kind stays. Fix comments
/// and replies are history the PR keeps, so they are never hidden.
pub fn comments_to_hide(comments: &[PrComment]) -> Vec<String> {
    let ours = |c: &&PrComment| c.body.starts_with(crate::render::MARKER);
    let newest = |kind: fn(&str) -> bool| comments.iter().filter(ours).filter(|c| kind(&c.body)).last().map(|c| c.id.clone());
    let keep = [newest(is_review_comment), newest(is_ci_comment)];
    comments.iter().filter(ours).filter(|c| is_review_comment(&c.body) || is_ci_comment(&c.body))
        .filter(|c| !c.hidden && !keep.contains(&Some(c.id.clone()))).map(|c| c.id.clone()).collect()
}

pub fn threads_to_reopen(threads: &[Thread], open: &HashSet<u64>) -> Vec<String> {
    threads.iter().filter(|t| t.resolved && t.first.is_some_and(|f| open.contains(&f))).map(|t| t.id.clone()).collect()
}

/// `revaiw sweep` targets: open PRs, narrowed to the named branches if any, and only where this machine has the
/// branch's ledger (without one, nothing says which threads are settled). Unknown names are reported back.
pub fn sweep_targets(prs: Vec<(String, Pr)>, want: &[String], has_ledger: &dyn Fn(&str) -> bool)
    -> (Vec<(String, Pr)>, Vec<String>) {
    let mut unknown: Vec<String> = want.iter().filter(|w| !prs.iter().any(|(b, _)| b == *w))
        .map(|w| format!("{w} (no open PR)")).collect();
    let mut go = vec![];
    for (b, pr) in prs.into_iter().filter(|(b, _)| want.is_empty() || want.contains(b)) {
        if has_ledger(&b) { go.push((b, pr)) } else { unknown.insert(0, format!("{b} (no ledger)")) }
    }
    (go, unknown)
}

/// One page-walked GraphQL connection on a PR: `field` is `reviewThreads` or `comments`, `node` its selection.
fn pr_nodes(slug: &str, pr: &Pr, field: &str, node: &str) -> Result<Vec<Value>, String> {
    let (owner, name) = slug.split_once('/').ok_or("bad slug")?;
    let q = format!("query($o:String!,$r:String!,$n:Int!,$c:String){{repository(owner:$o,name:$r){{pullRequest(number:$n){{\
                     {field}(first:100,after:$c){{pageInfo{{hasNextPage endCursor}} nodes{{{node}}}}}}}}}}}");
    let (mut all, mut cursor) = (vec![], None::<String>);
    loop {
        let mut args = vec!["api".to_string(), "graphql".into(), "-f".into(), format!("query={q}"), "-f".into(),
                            format!("o={owner}"), "-f".into(), format!("r={name}"), "-F".into(), format!("n={}", pr.number)];
        if let Some(c) = &cursor { args.extend(["-f".into(), format!("c={c}")]); }
        let out = gh(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
        let v: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        let conn = &v["data"]["repository"]["pullRequest"][field];
        all.extend(conn["nodes"].as_array().cloned().unwrap_or_default());
        if !conn["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false) { return Ok(all); }
        cursor = conn["pageInfo"]["endCursor"].as_str().map(str::to_string);
    }
}

pub struct Synced { pub resolved: usize, pub reopened: usize, pub hidden: usize, pub errors: Vec<String> }

/// Brings a PR in line with its ledger: resolves the threads of settled findings, reopens resolved threads of
/// live ones, and hides superseded revaiw comments as outdated. With `apply` false it only counts (a dry
/// run). A failed call is recorded and the rest carries on.
pub fn sync_pr(slug: &str, pr: &Pr, closed: &HashSet<u64>, open: &HashSet<u64>, apply: bool) -> Result<Synced, String> {
    let threads: Vec<Thread> = pr_nodes(slug, pr, "reviewThreads", "id isResolved comments(first:1){nodes{databaseId}}")?
        .iter().map(|t| Thread {
            id: t["id"].as_str().unwrap_or_default().into(),
            first: t["comments"]["nodes"][0]["databaseId"].as_u64(),
            resolved: t["isResolved"].as_bool().unwrap_or(true),
        }).collect();
    let comments: Vec<PrComment> = pr_nodes(slug, pr, "comments", "id body isMinimized")?.iter().map(|c| PrComment {
        id: c["id"].as_str().unwrap_or_default().into(),
        body: c["body"].as_str().unwrap_or_default().into(),
        hidden: c["isMinimized"].as_bool().unwrap_or(false),
    }).collect();
    let (resolve, reopen, hide) = (threads_to_resolve(&threads, closed), threads_to_reopen(&threads, open),
                                   comments_to_hide(&comments));
    let mut errors = vec![];
    if apply {
        let mut run = |m: &str, id: &str| {
            if let Err(e) = gh(&["api", "graphql", "-f", &format!("query={m}"), "-f", &format!("i={id}")], None) {
                errors.push(format!("{id}: {e}"));
            }
        };
        for t in &resolve { run("mutation($i:ID!){resolveReviewThread(input:{threadId:$i}){thread{id}}}", t); }
        for t in &reopen { run("mutation($i:ID!){unresolveReviewThread(input:{threadId:$i}){thread{id}}}", t); }
        for c in &hide {
            run("mutation($i:ID!){minimizeComment(input:{subjectId:$i,classifier:OUTDATED}){minimizedComment{isMinimized}}}", c);
        }
    }
    Ok(Synced { resolved: resolve.len(), reopened: reopen.len(), hidden: hide.len(), errors })
}

/// Open PRs as (head branch, PR).
pub fn open_prs() -> Result<Vec<(String, Pr)>, String> {
    let out = gh(&["pr", "list", "--state", "open", "--limit", "200", "--json",
                   "number,headRefName,baseRefName,isCrossRepository"], None)?;
    // Unparseable output is an error, not "no PRs": the inbox would otherwise drop every mention as not on a PR.
    serde_json::from_str::<Value>(&out).map_err(|e| format!("gh pr list: {e}"))?;
    Ok(parse_open_prs(&out))
}

/// The open same-repo PR's author and body, for its `## Rulings` (spec C3). `Ok(None)` when the branch has no open PR.
pub fn pr_body_author(branch: &str) -> Result<Option<(String, String)>, String> {
    let out = gh(&["pr", "list", "--head", branch, "--state", "open", "--json", "author,body,isCrossRepository",
                   "--limit", "10"], None)?;
    parse_pr_body_author(&out)
}

/// A fork's PR can share our branch's name and its body is the fork author's to write, so only same-repo PRs count.
fn parse_pr_body_author(json: &str) -> Result<Option<(String, String)>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("gh pr list: {e}"))?;
    Ok(v.as_array().into_iter().flatten().filter(|p| p["isCrossRepository"].as_bool() == Some(false))
        .find_map(|p| Some((p["author"]["login"].as_str()?.to_string(), p["body"].as_str().unwrap_or("").to_string()))))
}

/// The text under a `## Rulings` heading (any case) up to the next `## ` heading, at most 2000 characters. None when
/// there is no such section or it is empty.
pub fn rulings_section(body: &str) -> Option<String> {
    let mut lines = body.lines().skip_while(|l| !l.trim().eq_ignore_ascii_case("## rulings"));
    lines.next()?;
    let text: String = lines.take_while(|l| !l.starts_with("## ")).collect::<Vec<_>>().join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(2000).collect())
}

/// Same-repo PRs only: a fork's head branch can share a name with one of ours, and revaiw must never review or
/// push to ours on the fork's behalf.
pub fn parse_open_prs(json: &str) -> Vec<(String, Pr)> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    v.as_array().into_iter().flatten()
        .filter(|p| p["isCrossRepository"].as_bool() == Some(false))
        .filter_map(|p| Some((p["headRefName"].as_str()?.to_string(),
                              Pr { number: p["number"].as_u64()?, base: p["baseRefName"].as_str()?.to_string() })))
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Finding;
    use std::collections::{HashMap, HashSet};
    fn fnd(sev: &str, file: &str, line: u32, end: Option<u32>) -> Finding {
        Finding { severity: sev.into(), file: file.into(), line, end_line: end, summary: "s".into(), why: "w".into(),
                  snippet: String::new(), lang: String::new(), suggested_patch: None, suggestion: None, on_base: false }
    }
    fn run(id: u64, status: &str, conclusion: &str) -> CiRun {
        CiRun { id, name: format!("wf{id}"), status: status.into(), conclusion: conclusion.into() }
    }
    #[test] fn issue_comments_keep_only_pull_requests() {
        let json = r#"[
          {"id": 11, "body": "@review", "user": {"login": "kim"}, "created_at": "2026-10-15T10:00:00Z",
           "html_url": "https://github.com/o/r/pull/7#issuecomment-11", "issue_url": "https://api.github.com/repos/o/r/issues/7"},
          {"id": 12, "body": "@review", "user": {"login": "kim"}, "created_at": "2026-10-15T10:01:00Z",
           "html_url": "https://github.com/o/r/issues/8#issuecomment-12", "issue_url": "https://api.github.com/repos/o/r/issues/8"}
        ]"#;
        assert_eq!(parse_issue_comments(json), vec![IssueComment { id: 11, pr: 7, login: "kim".into(),
            body: "@review".into(), created_at: "2026-10-15T10:00:00Z".into() }]);
        assert!(parse_issue_comments("not json").is_empty());
    }
    #[test] fn review_comments_count_only_the_reactions_revaiw_acts_on() {
        let json = r#"[{"id": 5, "reactions": {"+1": 1, "-1": 1, "heart": 1, "confused": 0, "rocket": 3, "eyes": 2}}]"#;
        assert_eq!(parse_review_comments(json), vec![ReviewComment { id: 5, relevant: 3 }]);
    }
    #[test] fn reactions_come_oldest_first() {
        let json = r#"[{"id": 9, "content": "-1", "user": {"login": "b"}}, {"id": 3, "content": "+1", "user": {"login": "a"}}]"#;
        let r = parse_reactions(json);
        assert_eq!((r[0].id, r[0].login.as_str(), r[1].content.as_str()), (3, "a", "-1"));
    }
    #[test] fn gh_runs_under_a_timeout() {
        let c = gh_command(&["api", "x"]);
        assert_eq!(c.get_program(), "timeout");
        let a: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(&a[..5], &["-k", "5", "60", "gh", "api"]);
    }
    #[test] fn sync_resolves_only_threads_the_ledger_closed() {
        let t = |id: &str, first: u64, resolved: bool| Thread { id: id.into(), first: Some(first), resolved };
        let threads = vec![t("A", 1, false), t("B", 2, false), t("C", 3, true), Thread { id: "D".into(), first: None, resolved: false }];
        let closed: HashSet<u64> = [1, 3].into_iter().collect();
        // 1 closed and open → resolve; 2 still open in the ledger → keep; 3 already resolved; D is not ours.
        assert_eq!(threads_to_resolve(&threads, &closed), vec!["A".to_string()]);
    }
    #[test] fn ci_detection_matches_what_the_renderer_really_writes() {
        use crate::{ledger::Ledger, model::ReviewOutput, render::{summary, Ctx}};
        let (l, out) = (Ledger::default(), ReviewOutput::default());
        let body = |title: &str| summary(&Ctx { title, branch: "b", base: "a", head: "h", model: "m", effort: "e", reason: "r",
                                                  out: &out, ledger: &l, fixed_now: &[], engine: None, follow_up: false });
        assert!(is_ci_comment(&body("🧪 CI failures")));
        assert!(!is_ci_comment(&body("🐇 Review")));
    }
    #[test] fn sync_reopens_threads_the_ledger_has_open_again() {
        let t = |id: &str, first: u64, resolved: bool| Thread { id: id.into(), first: Some(first), resolved };
        let threads = vec![t("A", 1, true), t("B", 2, true), t("C", 3, false)];
        let open: HashSet<u64> = [1, 3].into_iter().collect();
        assert_eq!(threads_to_reopen(&threads, &open), vec!["A".to_string()]);
    }
    #[test] fn sweep_targets_only_branches_with_a_ledger_and_names_unknown_ones() {
        let prs = vec![("feat/a".to_string(), Pr { number: 1, base: "uat".into() }),
                       ("feat/b".to_string(), Pr { number: 2, base: "uat".into() })];
        let has = |b: &str| b == "feat/a";
        let (go, unknown) = sweep_targets(prs, &["feat/a".into(), "feat/b".into(), "feat/typo".into()], &has);
        assert_eq!(go.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(), vec!["feat/a"]);
        assert_eq!(unknown, vec!["feat/b (no ledger)".to_string(), "feat/typo (no open PR)".to_string()]);
    }
    #[test] fn sync_keeps_only_the_newest_review_and_ci_comment_visible() {
        let c = |id: &str, title: &str, hidden: bool| PrComment { id: id.into(), hidden,
            body: format!("<!-- revaiw -->\n## {title} · `b` @ `x`\n") };
        let comments = vec![c("r1", "🐇 Review", false), c("ci1", "🧪 CI failures", false), c("r2", "🐇 Review", true),
                            c("r3", "🐇 Review", false), PrComment { id: "h".into(), hidden: false, body: "a human".into() },
                            c("ci2", "🧪 CI failures", false)];
        // r1 and ci1 are superseded; r2 is already hidden; r3 and ci2 are the newest; the human comment is not ours.
        assert_eq!(comments_to_hide(&comments), vec!["r1".to_string(), "ci1".to_string()]);
        // A review that merely quotes the CI heading is still a review: its kind is its own heading line.
        let mut quoting = c("r4", "🐇 Review", false);
        quoting.body.push_str("as the earlier ## 🧪 CI failures comment said\n");
        let both = vec![c("r3", "🐇 Review", false), c("ci2", "🧪 CI failures", false), quoting];
        assert_eq!(comments_to_hide(&both), vec!["r3".to_string()], "r4 is the newest review, ci2 stays");
    }
    #[test] fn a_fix_comment_or_a_reply_never_hides_the_review_summary_nor_is_hidden() {
        let m = |id: &str, rest: &str| PrComment { id: id.into(), hidden: false, body: format!("<!-- revaiw -->\n{rest}") };
        let review = |id: &str| m(id, "## 🐇 Review · `b` @ `x`\n");
        let fix = || m("f1", "## 🔧 Fix · `b` @ `x`\n\nPushed.");
        let reply = || m("p1", "revaiw could not review `b`: nothing reviewable in the range.");
        // The review, then a fix and a reply: all three stay.
        assert!(comments_to_hide(&[review("r1"), fix(), reply()]).is_empty());
        // Two reviews and a fix after: only the older review goes.
        assert_eq!(comments_to_hide(&[review("r1"), review("r2"), fix()]), vec!["r1".to_string()]);
        // Fixes and replies pile up without ever being hidden, even as reviews are replaced.
        let all = vec![review("r1"), fix(), reply(), review("r2"), m("f2", "## 🔧 Fix · `b` @ `y`\n")];
        assert_eq!(comments_to_hide(&all), vec!["r1".to_string()]);
    }
    #[test] fn status_text_never_fails_the_commit() {
        assert_eq!(status_text(0, 0, 0), Some(("success", "No open findings".to_string())));
        assert_eq!(status_text(2, 1, 0), Some(("success", "2 important · 1 nit".to_string())));
    }
    #[test] fn a_partial_review_posts_no_status() {
        // No status already means "not reviewed"; a partial review must not read as a green one.
        assert_eq!(status_text(0, 0, 3), None);
    }
    #[test] fn ci_state_waits_then_reports() {
        assert!(matches!(ci_state(&[]), CiState::Pending));
        assert!(matches!(ci_state(&[run(1, "completed", "success"), run(2, "in_progress", "")]), CiState::Pending));
        assert!(matches!(ci_state(&[run(1, "completed", "success"), run(2, "completed", "skipped")]), CiState::Passed));
        // A draft PR: every workflow skips itself, so nothing actually ran. That is not green.
        assert!(matches!(ci_state(&[run(1, "completed", "skipped"), run(2, "completed", "skipped")]), CiState::Skipped));
        // Marked ready: real runs start on the same commit beside the skipped ones.
        assert!(matches!(ci_state(&[run(1, "completed", "skipped"), run(3, "in_progress", "")]), CiState::Pending));
        match ci_state(&[run(1, "completed", "failure"), run(2, "completed", "success"), run(3, "completed", "timed_out")]) {
            CiState::Failed(f) => assert_eq!(f.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]),
            _ => panic!("expected Failed"),
        }
    }
    #[test] fn log_tail_keeps_the_end() {
        let log: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let t = log_tail(&log, 150, 100_000);
        assert!(t.starts_with("line 350") && t.ends_with("line 499"));
        assert!(log_tail(&log, 1000, 50).len() <= 50);
    }
    #[test] fn multi_line_suggestion_dropped_without_start_line() {
        let added: HashMap<String, HashSet<u32>> = [("a.ts".to_string(), [12u32].into_iter().collect())].into();
        let mut f = fnd("important", "a.ts", 10, Some(12));
        f.suggestion = Some("x".into());
        let (v, _) = review_payload("sha", &[("i".into(), f)], &added).unwrap();
        let body = v["comments"][0]["body"].as_str().unwrap();
        assert!(!body.contains("```suggestion"), "{body}");
        assert!(v["comments"][0].get("start_line").is_none());
    }
    #[test] fn pairs_comment_ids_by_position() {
        let payload = json!({"comments": [{"path": "a.ts", "line": 5}, {"path": "a.ts", "line": 5}, {"path": "b.ts", "line": 1}]});
        let posted = vec![json!({"id": 11, "path": "a.ts", "line": 5}), json!({"id": 12, "path": "a.ts", "line": 5}),
                          json!({"id": 13, "path": "b.ts", "line": 1})];
        assert_eq!(pair_ids(&payload, &posted), vec![Some(11), Some(12), Some(13)]);
        assert_eq!(pair_ids(&payload, &posted[..1]), vec![Some(11), None, None]);
    }
    #[test] fn payload_only_important_on_added_lines() {
        let added: HashMap<String, HashSet<u32>> = [("a.ts".to_string(), [10u32, 11, 12].into_iter().collect())].into();
        let items = vec![
            ("i1".to_string(), fnd("important", "a.ts", 10, Some(12))),
            ("i2".to_string(), fnd("important", "a.ts", 99, None)),
            ("i3".to_string(), fnd("nit", "a.ts", 11, None)),
            ("i4".to_string(), fnd("important", "b.ts", 1, None)),
        ];
        let (v, ids) = review_payload("sha", &items, &added).unwrap();
        assert_eq!(ids, vec!["i1".to_string()]);
        assert_eq!(v["event"], "COMMENT");
        assert_eq!(v["commit_id"], "sha");
        let c = &v["comments"][0];
        assert_eq!((c["path"].as_str(), c["line"].as_u64(), c["start_line"].as_u64()), (Some("a.ts"), Some(12), Some(10)));
        assert!(review_payload("sha", &items[1..], &added).is_none());
    }
    #[test] fn open_prs_skip_forks() {
        let json = r#"[{"number":7,"headRefName":"feat/x","baseRefName":"main","isCrossRepository":false},
                       {"number":8,"headRefName":"feat/x","baseRefName":"main","isCrossRepository":true}]"#;
        let prs = parse_open_prs(json);
        assert_eq!(prs.iter().map(|(b, p)| (b.as_str(), p.number, p.base.as_str())).collect::<Vec<_>>(),
                   vec![("feat/x", 7, "main")], "a fork's same-named branch is not ours");
    }
    #[test] fn rulings_section_runs_to_the_next_heading_or_the_end() {
        let b = "## Summary\nx\n## Rulings\n- one bad runner refuses the card\n\n## Test plan\ny";
        assert_eq!(rulings_section(b).as_deref(), Some("- one bad runner refuses the card"));
        assert_eq!(rulings_section("## rulings\nlast").as_deref(), Some("last"));
        assert_eq!(rulings_section("## Summary\nno rulings here"), None);
        assert_eq!(rulings_section("## Rulings\n\n## Next"), None, "an empty section is none");
        assert!(rulings_section(&format!("## Rulings\n{}", "x".repeat(5000))).unwrap().chars().count() <= 2000);
    }
    #[test] fn pr_body_author_takes_the_first_same_repo_pr() {
        let json = r###"[{"author":{"login":"fork"},"body":"f","isCrossRepository":true},
                       {"author":{"login":"kim"},"body":"## Rulings\nx","isCrossRepository":false}]"###;
        assert_eq!(parse_pr_body_author(json), Ok(Some(("kim".into(), "## Rulings\nx".into()))));
        assert_eq!(parse_pr_body_author(r#"[{"author":{"login":"fork"},"body":"f","isCrossRepository":true}]"#), Ok(None));
        assert_eq!(parse_pr_body_author("[]"), Ok(None));
        assert!(parse_pr_body_author("not json").is_err());
    }
}
