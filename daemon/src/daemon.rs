use crate::{allocate, config::{self, Config}, filter, fix, git::Repo, github, ledger::{Ledger, LedgerEntry},
            queue::{Entry, FixJob, Queue, Retry}, render, review, route::{self, Route}, status::{Done, Run, Status}};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds without another push before a branch is reviewed. `REVAIW_DEBOUNCE` overrides it (the e2e test uses 1).
fn debounce() -> u64 {
    std::env::var("REVAIW_DEBOUNCE").ok().and_then(|v| v.parse().ok()).unwrap_or(90)
}
const CI_POLL: u64 = 60;
const CI_GIVE_UP: u64 = 3600;
/// A draft's CI is skipped until it is marked ready, which can be hours away: wait longer, poll slower.
const DRAFT_POLL: u64 = 300;
const DRAFT_GIVE_UP: u64 = 86_400;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

pub fn state_dir(repo: &Repo) -> PathBuf {
    repo.common.join("revaiw")
}

/// Desktop notifications are off unless the `notify` marker exists (`revaiw notify on`). Checked at each use.
pub fn notify_enabled(state_dir: &std::path::Path) -> bool {
    state_dir.join("notify").exists()
}

/// The resource guard (hold jobs while the box is busy) is off unless the `guard` marker exists (`revaiw guard on`).
/// Checked each tick.
pub fn guard_enabled(state_dir: &std::path::Path) -> bool {
    state_dir.join("guard").exists()
}

macro_rules! log { ($($t:tt)*) => { eprintln!("[{}] {}", now(), format!($($t)*)) } }

#[derive(PartialEq)]
enum Kind {
    /// Stage 0 (`checks.on_review`): the configured checks in the review's worktree; their output feeds the engine.
    Checks,
    /// Stage 1: `/code-review` in a worktree of the head.
    Engine,
    /// Stage 2: Haiku formats the engine's findings; its output is the review that gets recorded.
    Review,
    Ci,
    /// Fix stage 1: the fix engine edits U, its own worktree of the head.
    Fix,
    /// Fix stage 2: the configured checks run in C, the clean checkout the fix was harvested into.
    FixChecks,
}

/// A reviewed head whose GitHub Actions runs are awaited; failures become a CI job.
struct CiWatch {
    head: String,
    /// Already told the conversation this head's CI is skipped (draft); keep polling for real runs.
    told_skipped: bool,
    since: u64,
    polled: u64,
    /// Open important findings when this head's review was recorded; decides ✅ without a status lookup.
    important: usize,
    /// Reads the head's last review was refused: a partial review is never ✅.
    refused: usize,
}

struct CiJob {
    branch: String,
    head: String,
    failures: Vec<(String, String)>,
}

struct Job {
    kind: Kind,
    branch: String,
    entry: Entry,
    route: Route,
    child: Child,
    out: PathBuf,
    ci: Option<CiJob>,
    /// When the CLI was spawned; with `entry.queued_at` (the last push) and the finish time it gives the
    /// timing split on the verdict line. The formatter keeps the engine's, so the split covers both stages.
    started: u64,
    /// The review's detached checkout of the head, removed once the review is recorded or abandoned.
    worktree: Option<PathBuf>,
    /// Tool calls the engine was refused (`tool: command`): something it could not read, so the review is partial.
    refused: Vec<String>,
    /// A fix job: the request, its selected findings, and the files the harvest changed in C (empty until then).
    fix: Option<(FixJob, Vec<(String, LedgerEntry)>, Vec<String>)>,
}

/// The line to log when the resource guard refuses a due review: once when a hold starts, not on every 5 s tick of it.
fn hold_line(holding: &mut bool, refused: bool, g: &allocate::Guard) -> Option<String> {
    let start = refused && !*holding;
    *holding = refused;
    start.then(|| format!("holding: resource guard (avail {:.1} GiB, load {:.2}, heavy {})", g.avail, g.load1, g.heavy))
}

/// The config the running daemon uses after a reload, and the load error to report (once, by the caller). A config
/// that could not be read or parsed never replaces the last good one: `load` hands back defaults then, and those
/// would silently turn the repo's settings off.
fn next_config(prev: &Config, l: config::Loaded) -> (Config, Option<String>) {
    let keep = !l.trunk_ok || l.err.is_some();
    (if keep { prev.clone() } else { l.cfg }, l.err)
}

/// Seconds between attempts to learn the GitHub repo while `gh` cannot say.
const SLUG_RETRY: u64 = 60;

/// The inbox's GitHub repo, looked up when the inbox is first due rather than once at start: a `gh` failure then
/// (offline, not yet logged in) must not turn mentions off for the daemon's whole life.
#[derive(Default)]
struct Slug {
    live: Option<crate::inbox::Live>,
    err: Option<String>,
    next: u64,
}

impl Slug {
    /// Tries `lookup` while the repo is unknown, at most every `SLUG_RETRY` s. Some(line to log) on a failure that
    /// differs from the last one, so a lasting failure is logged once.
    fn refresh(&mut self, t: u64, lookup: impl FnOnce() -> Result<String, String>) -> Option<String> {
        if self.live.is_some() || t < self.next { return None; }
        match lookup() {
            Ok(slug) => { self.live = Some(crate::inbox::Live { slug }); self.err = None; None }
            Err(e) => {
                self.next = t + SLUG_RETRY;
                let line = (self.err.as_deref() != Some(e.as_str())).then(|| format!("inbox: no GitHub repo slug yet: {e}; retrying"));
                self.err = Some(e);
                line
            }
        }
    }
}

/// Idles on inotify (remote-tracking refs only) with a 5 s tick; two review children at most (three with
/// `REVAIW_SLOTS=3`).
pub fn run(repo: Repo) {
    let dir = state_dir(&repo);
    let (qp, sp) = (dir.join("queue.json"), dir.join("status.json"));
    fs::create_dir_all(dir.join("runs")).unwrap();
    let mut q = Queue::load(&qp);
    let mut st = Status::load(&sp);
    if let Err(e) = repo.exclude_reviews() { log!("could not add /.reviews/ to info/exclude: {e}"); }
    // Until the trunk can be read, the defaults (the safe set); after that a failure keeps the last good config.
    let l = config::load(&repo);
    let (mut cfg, mut cfg_err) = (l.cfg, l.err);
    if let Some(e) = &cfg_err { log!("config: {e}; using defaults"); }
    st.pid = std::process::id();
    // Reviews run in their own process group and outlive a crashed daemon; stop them before requeueing.
    for r in &st.running {
        let cmd = fs::read_to_string(format!("/proc/{}/cmdline", r.pid)).unwrap_or_default();
        if ["nice", "timeout", "claude"].iter().any(|n| cmd.contains(n)) {
            let _ = std::process::Command::new("kill").args(["-TERM", "--", &format!("-{}", r.pid)]).status();
            log!("killed orphaned review pgid {} ({})", r.pid, r.branch);
        }
    }
    st.running.clear();
    st.save(&sp);

    let (tx, rx) = mpsc::channel();
    // Ignore reads: the snapshot's own `git for-each-ref` reads these directories, and forwarding those access
    // events re-triggered the snapshot in a loop (~300k events per 6 s, ~13% CPU idle). A push creates/renames.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(e) = res {
            if !matches!(e.kind, notify::EventKind::Access(_)) { let _ = tx.send(()); }
        }
    }).expect("watcher");
    use notify::{RecursiveMode, Watcher};
    let refs = repo.common.join("refs/remotes").join(&repo.remote);
    let _ = fs::create_dir_all(&refs);
    // Only the remote-tracking refs: a push writes a loose ref here, and packing deletes it (also an event).
    // Not the git dir itself: every git command from every agent touches its index there, which flooded the
    // watcher (~15% CPU idle). The 30 s fallback snapshot covers anything missed.
    watcher.watch(&refs, RecursiveMode::Recursive).expect("watch refs");

    let mut snap = loop {
        match repo.snapshot() {
            Some(s) => break s,
            None => { log!("snapshot failed; retrying"); std::thread::sleep(Duration::from_secs(5)); }
        }
    };
    let mut jobs: Vec<Job> = vec![];
    let mut watches: std::collections::BTreeMap<String, CiWatch> = Default::default();
    let mut ci_due: Vec<CiJob> = vec![];
    // Fixes whose engine is done and change is guarded, waiting for the resource guard to allow their checks child.
    let mut pending_checks: Vec<Job> = vec![];
    let missing = review::missing_sandbox_tools();
    if !missing.is_empty() {
        log!("{}", review::sandbox_hint(&missing));
    }
    log!("started pid {} watching {} ({} branches, {} slots)", st.pid, refs.display(), snap.len(),
         allocate::slots_from(std::env::var("REVAIW_SLOTS").ok().as_deref()));

    let slots = allocate::slots_from(std::env::var("REVAIW_SLOTS").ok().as_deref());
    let mut inbox = crate::inbox::Inbox::load(&dir, &cfg, now());
    let mut slug = Slug::default();
    let mut tick: u64 = 0;
    let mut holding = false;
    loop {
        let mut dirty = rx.recv_timeout(Duration::from_secs(5)).is_ok();
        while rx.try_recv().is_ok() { dirty = true; }
        let t = now();
        let mut changed = false;
        let guard_on = guard_enabled(&dir);
        if !guard_on { holding = false; }

        tick += 1;
        // inotify can lose the refs watch (gc prunes/recreates dirs); a 30 s re-snapshot catches anything missed.
        if dirty || tick % 6 == 0 {
          // Re-read the trunk's config on the same cadence: a merged config change applies within 30 s.
          let (next, err) = next_config(&cfg, config::load(&repo));
          if err != cfg_err {
              if let Some(e) = &err { log!("config: {e}; keeping the last config"); }
              cfg_err = err;
          }
          cfg = next;
          if let Some(next) = repo.snapshot() {
            for (b, sha) in &next {
                if snap.get(b) == Some(sha) || filter::skip_branch(&cfg, b) || !cfg.triggers.push { continue; }
                enqueue(&repo, &cfg, &mut q, b, snap.get(b).map(String::as_str), sha, t);
                changed = true;
            }
            for b in snap.keys().filter(|b| !next.contains_key(*b)) { q.remove(b); changed = true; }
            snap = next;
          }
        }

        if let Some(body) = take_requests(&dir.join("now-requests")) {
            for (b, tier) in body.lines().map(route::parse_now_line).filter(|(b, _)| !b.is_empty()) {
                let tier = tier.unwrap_or_else(|t| { log!("now: unknown model {t} for {b}, routing by size"); None });
                match snap.get(&b) {
                    Some(sha) => { let sha = sha.clone(); changed |= enqueue_now(&repo, &cfg, &mut q, &b, &sha, tier, None); }
                    None => log!("now: no remote branch {b}"),
                }
            }
        }

        // @review mentions and reactions. Needs a GitHub remote and posting on: a local-only run answers nobody.
        if github::posting_enabled() && inbox.due(t) {
            if let Some(line) = slug.refresh(t, github::try_repo_slug) { log!("{line}"); }
        }
        if let Some(gh) = slug.live.as_ref().filter(|_| github::posting_enabled() && inbox.due(t)) {
            for a in inbox.poll(gh, &cfg, t, &|b| ledger_path(&repo, b)) {
                match a {
                    // 👀 only once queued (spec §5.2.4); a request that cannot be queued gets a reply saying why.
                    crate::inbox::Action::Review { branch, pr, model, request } => {
                        let id = request.comment_id;
                        let dropped = match snap.get(&branch) {
                            Some(sha) => {
                                let sha = sha.clone();
                                log!("{branch}: review requested by @{} (#{id})", request.login);
                                if enqueue_now(&repo, &cfg, &mut q, &branch, &sha, model, Some(request)) { None }
                                else { Some("nothing reviewable in the range") }
                            }
                            None => { log!("mention: no remote branch {branch}"); Some("this clone has not fetched that branch") }
                        };
                        match dropped {
                            None => { github::add_reaction(&gh.slug, id, "eyes"); changed = true; }
                            Some(reason) => {
                                let body = format!("{}\nrevaiw could not review `{branch}`: {reason}.", render::MARKER);
                                if let Err(e) = github::post_issue_comment(&gh.slug, pr, &body) { log!("comment on #{pr}: {e}"); }
                            }
                        }
                    }
                    crate::inbox::Action::Fix { branch, pr: _, ids, login, comment_id } => {
                        log!("{branch}: fix requested by @{login}{}", if ids.is_empty() { String::new() } else { format!(" for {}", ids.join(" ")) });
                        q.push_fix(&branch, ids, &login, comment_id, t);
                        if let Some(id) = comment_id { github::add_reaction(&gh.slug, id, "eyes"); }
                        changed = true;
                    }
                }
            }
        }

        let mut i = 0;
        while i < jobs.len() {
            match jobs[i].child.try_wait() {
                Ok(Some(status)) => {
                    let j = jobs.remove(i);
                    match j.kind {
                        // Evidence only: a failing check never stops the review, the engine weighs it.
                        Kind::Checks => {
                            st.running.retain(|r| !(r.branch == j.branch && r.pid == j.child.id()));
                            kill_group(j.child.id()); // nothing a check started may keep running beside the engine
                            let checks = fs::read_to_string(&j.out).unwrap_or_default();
                            let _ = fs::remove_file(&j.out);
                            let wt = j.worktree.clone().expect("a checks job has a worktree");
                            match start_engine(&repo, &cfg, &j.branch, j.entry.clone(), j.route, wt, checks) {
                                Some(next) => {
                                    st.running.push(Run { branch: next.branch.clone(), head: next.entry.head.clone(),
                                                          model: next.route.model.clone(), pid: next.child.id(), started_at: t });
                                    jobs.push(next);
                                }
                                None => { st.failed += 1; q.finish(&j.branch); }
                            }
                        }
                        Kind::Engine => if let Some(next) = advance(&repo, &cfg, &mut q, &mut st, j, t) {
                            st.running.push(Run { branch: next.branch.clone(), head: next.entry.head.clone(),
                                                  model: format!("{} · format", cfg.models.small), pid: next.child.id(), started_at: t });
                            jobs.push(next);
                        },
                        Kind::Fix => if let Some(next) = fix_checked(&repo, &cfg, &mut q, &mut st, j, t) {
                            pending_checks.push(next);
                        },
                        Kind::FixChecks => fix_checks_done(&repo, &cfg, &mut q, &mut st, j, status),
                        Kind::Review | Kind::Ci => if let Some((b, w)) = finish(&repo, &cfg, &mut q, &mut st, &mut ci_due, j, t) {
                            watches.insert(b, w);
                        },
                    }
                    changed = true;
                }
                _ => i += 1,
            }
        }

        // A checks child is a job like any other: it starts only when the resource guard allows (spec §5.4.4). Until
        // then (or while the guard cannot be read) the fix waits here with U, C and its harvested files intact.
        while !pending_checks.is_empty() {
            if !allocate::admit(guard_on, jobs.len(), slots, allocate::read_guard).go() { break; }
            if let Some(j) = start_checks(&repo, &cfg, &mut q, &mut st, pending_checks.remove(0)) {
                st.running.push(Run { branch: j.branch.clone(), head: j.entry.head.clone(), model: "fix · checks".into(),
                                      pid: j.child.id(), started_at: t });
                jobs.push(j);
            }
            changed = true;
        }

        let poll_every = |w: &CiWatch| if w.told_skipped { DRAFT_POLL } else { CI_POLL };
        let give_up = |w: &CiWatch| if w.told_skipped { DRAFT_GIVE_UP } else { CI_GIVE_UP };
        for (b, w) in watches.iter_mut().filter(|(_, w)| t.saturating_sub(w.polled) >= poll_every(w)) {
            w.polled = t;
            match github::ci_runs(&w.head).map(|r| github::ci_state(&r)) {
                Some(github::CiState::Failed(runs)) => {
                    let failures = runs.iter().map(|r| (r.name.clone(), github::failed_log(r.id))).collect();
                    log!("{b}: {} CI run(s) failed on {}", runs.len(), &w.head[..7]);
                    ci_due.push(CiJob { branch: b.clone(), head: w.head.clone(), failures });
                    w.since = 0; // done
                }
                Some(github::CiState::Skipped) => {
                    // A draft: keep watching, since marking it ready starts real runs on this same commit.
                    if !w.told_skipped {
                        log!("{b}: CI skipped @{} (draft PR?), not ready to merge until CI runs", &w.head[..7]);
                        w.told_skipped = true;
                    }
                }
                Some(github::CiState::Passed) => {
                    // A watch only exists once its head's review is recorded (finish), so the count is always known.
                    match (w.important, w.refused) {
                        (0, 0) => log!("{b}: ✅ ready to merge @{} (review clean, CI green)", &w.head[..7]),
                        (0, _) => log!("{b}: CI green @{}, review partial (see its comment)", &w.head[..7]),
                        (n, _) => log!("{b}: CI green @{}, {n} important still open", &w.head[..7]),
                    }
                    w.since = 0;
                }
                _ => {}
            }
        }
        for (b, w) in watches.iter().filter(|(_, w)| w.since != 0 && t.saturating_sub(w.since) >= give_up(w)) {
            log!("{b}: no CI result @{} after {} h, stopped waiting (revaiw now {b} re-checks)", &w.head[..7], give_up(w) / 3600);
        }
        watches.retain(|_, w| w.since != 0 && t.saturating_sub(w.since) < give_up(w));

        while t >= st.paused_until && !ci_due.is_empty() {
            if !allocate::admit(guard_on, jobs.len(), slots, allocate::read_guard).go() { break; }
            let c = ci_due.remove(0);
            if let Some(j) = start_ci(&repo, &cfg, c) {
                st.running.push(Run { branch: j.branch.clone(), head: j.entry.head.clone(), model: format!("ci:{}", j.route.model),
                                      pid: j.child.id(), started_at: t });
                jobs.push(j);
            }
            changed = true;
        }

        while t >= st.paused_until {
            // Only ask the guard (a python spawn) when something is actually due.
            if !q.entries.iter().any(|(b, e)| !q.running.contains_key(b) && t.saturating_sub(e.queued_at) >= debounce()) { break; }
            match allocate::admit(guard_on, jobs.len(), slots, allocate::read_guard) {
                allocate::Admit::Go => holding = false,
                allocate::Admit::Full => break,
                allocate::Admit::Held(g) => { if let Some(line) = hold_line(&mut holding, true, &g) { log!("{line}"); } break; }
                allocate::Admit::Unreadable => { log!("resource-guard and /proc unreadable; holding"); break; }
            }
            let Some((b, e)) = q.take_eligible(t, debounce()) else { break };
            match start(&repo, &cfg, &b, e.clone()) {
                Some(j) => {
                    st.running.push(Run { branch: b.clone(), head: e.head.clone(), model: j.route.model.clone(),
                                          pid: j.child.id(), started_at: t });
                    log!("review {b} {}..{} on {} with /code-review {} ({})", &e.base[..7], &e.head[..7], j.route.model,
                         j.route.level, j.route.reason);
                    jobs.push(j);
                }
                None => match q.retry(&b, e, t) {
                    Retry::Queued => log!("{b}: review could not start, retrying after the debounce"),
                    Retry::Superseded => log!("{b}: review could not start; the newer push queued meanwhile replaces it"),
                    Retry::GaveUp => {
                        st.failed += 1;
                        log!("{b}: review could not start 3 times, gave up (revaiw now {b} re-runs it)");
                    }
                },
            }
            changed = true;
        }

        // Requested fixes. Only ask the guard when one can be taken: `take_fix` never starts one beside its branch's
        // review, nor a retried one before its wait is over.
        while t >= st.paused_until && q.fix_due(t) {
            if !allocate::admit(guard_on, jobs.len(), slots, allocate::read_guard).go() { break; }
            // The head as of now, not the tick's snapshot: a push that landed while a fix ran must not be fixed stale.
            let Some((b, f, head)) = q.take_fix(t, &|b| repo.remote_head(b)) else { break };
            changed = true;
            // The request was gated on `fix.enabled` when it came in; the base's config may have turned it off since.
            if !cfg.fix.enabled { log!("{b}: fix requested by @{}, but fix.enabled is now off; dropped", f.login); q.finish(&b); continue; }
            // Same for the trunk list: a request may predate the config that names this branch as one.
            if cfg.is_trunk(&b) { log!("{b}: fix requested by @{}, but {b} is a trunk branch; dropped", f.login); q.finish(&b); continue; }
            match start_fix(&repo, &cfg, &b, f.clone(), &head) {
                Some(j) => {
                    st.running.push(Run { branch: b.clone(), head: head.clone(), model: format!("fix:{}", cfg.fix.model),
                                          pid: j.child.id(), started_at: t });
                    jobs.push(j);
                }
                None => {
                    if fix::select(&Ledger::load(&ledger_path(&repo, &b)), &f.ids).is_empty() { q.finish(&b); }
                    else if q.retry_fix(&b, f, t) == Retry::GaveUp { log!("{b}: fix could not start 3 times, gave up"); }
                }
            }
        }

        if changed { q.save(&qp); st.save(&sp); }
    }
}

fn enqueue(repo: &Repo, cfg: &Config, q: &mut Queue, b: &str, old: Option<&str>, sha: &str, t: u64) {
    let Some(base) = repo.range(old, sha, &cfg.branches.base) else { log!("{b}: no range, skipped"); return };
    let files = repo.numstat(&base, sha);
    if files.iter().all(|(_, _, p)| filter::is_generated(cfg, p)) { log!("{b}: nothing reviewable (empty or generated-only), skipped"); return; }
    q.push(b, &base, sha, t, &|a, b| repo.is_ancestor(a, b));
}

/// `revaiw now`: the whole branch against the base branch, whatever is pending or running. False when nothing is
/// reviewable.
fn enqueue_now(repo: &Repo, cfg: &Config, q: &mut Queue, b: &str, sha: &str, tier: Option<String>, request: Option<crate::queue::Request>) -> bool {
    let Some(base) = repo.range(None, sha, &cfg.branches.base) else { log!("now: {b} has no range, skipped"); return false };
    if repo.numstat(&base, sha).iter().all(|(_, _, p)| filter::is_generated(cfg, p)) {
        log!("now: {b} has nothing reviewable (empty or generated-only), skipped");
        return false;
    }
    q.push_now(b, &base, sha, tier, request);
    true
}

/// Takes the pending `now-requests` by renaming it first, so a CLI append that lands meanwhile goes to a fresh
/// file for the next tick instead of being deleted unread.
fn take_requests(req: &std::path::Path) -> Option<String> {
    let taken = req.with_extension("taken");
    fs::rename(req, &taken).ok()?;
    let body = fs::read_to_string(&taken).ok();
    let _ = fs::remove_file(&taken);
    body
}

fn spawn(mut cmd: std::process::Command, out: &std::path::Path, b: &str) -> Option<Child> {
    let file = fs::File::create(out).ok()?;
    use std::os::unix::process::CommandExt;
    cmd.process_group(0).stdin(Stdio::null()).stdout(file).stderr(Stdio::null());
    cmd.spawn().map_err(|err| log!("{b}: spawn failed: {err}")).ok()
}

/// A review's checkout and the session folder its engine wrote into both go once the review is done.
fn cleanup(repo: &Repo, wt: &std::path::Path) {
    repo.remove_worktree(wt);
    let _ = fs::remove_dir_all(review::session_dir(&PathBuf::from(std::env::var("HOME").unwrap_or_default()), wt));
}

fn run_tag(b: &str, head: &str) -> String {
    format!("{}-{}", b.replace('/', "__"), &head[..7])
}

/// Stage 0/1: a fresh worktree of the head; then the configured checks first (`checks.on_review`), or straight to
/// the engine.
fn start(repo: &Repo, cfg: &Config, b: &str, e: Entry) -> Option<Job> {
    let files = repo.numstat(&e.base, &e.head);
    let route = e.model.as_deref().and_then(|t| route::pinned(cfg, t))
        .unwrap_or_else(|| route::route(cfg, &files, &repo.messages(&e.base, &e.head)));
    let runs = state_dir(repo).join("runs");
    let wt = runs.join(format!("wt-{}", run_tag(b, &e.head)));
    // Steer files from the trunk, never `e.base`: for a follow-up push that is the PR's own previous commit.
    let Some(steer) = repo.steer_base(&e.head, &cfg.branches.base) else {
        log!("{b}: no trunk ({}/{}) to take the reviewer's guidance from; not reviewed", repo.remote, cfg.branches.base);
        return None;
    };
    if !repo.add_worktree(&wt, &steer, &e.head) { log!("{b}: could not check out {} for review", &e.head[..7]); return None; }
    let _ = fs::create_dir_all(review::session_dir(&PathBuf::from(std::env::var("HOME").unwrap_or_default()), &wt));
    if cfg.checks.on_review && !cfg.checks.commands.is_empty() {
        let out = runs.join(format!("{}-checks.txt", run_tag(b, &e.head)));
        let cmd = fix::checks_command(&wt, &cfg.checks.commands, cfg.checks.timeout_secs);
        let Some(child) = spawn(cmd, &out, b) else { cleanup(repo, &wt); return None };
        log!("checks {b} @{} before review", &e.head[..7]);
        return Some(Job { kind: Kind::Checks, branch: b.into(), entry: e, route, child, out, ci: None, started: now(),
                          worktree: Some(wt), refused: vec![], fix: None });
    }
    start_engine(repo, cfg, b, e, route, wt, String::new())
}

/// Stage 1: `/code-review` at the routed level, guided by REVIEW.md, the diff's skills, a mention's request and any
/// check results.
fn start_engine(repo: &Repo, cfg: &Config, b: &str, e: Entry, route: Route, wt: PathBuf, checks: String) -> Option<Job> {
    let guidance = fs::read_to_string(wt.join("REVIEW.md")).unwrap_or_default()
        + &review::skill_guidance(cfg, &repo.files_at_head(&e.base, &e.head))
        + &e.request.as_ref().map(review::request_block).unwrap_or_default()
        + &review::checks_block(&checks);
    let out = state_dir(repo).join("runs").join(format!("{}-engine.json", run_tag(b, &e.head)));
    let graphify = cfg.review.graphify && wt.join("graphify-out/graph.json").exists();
    let Some(child) = spawn(review::engine_command(&wt, &route, &e.base, &e.head, &guidance, graphify), &out, b) else {
        cleanup(repo, &wt);
        return None;
    };
    Some(Job { kind: Kind::Engine, branch: b.into(), entry: e, route, child, out, ci: None, started: now(),
               worktree: Some(wt), refused: vec![], fix: None })
}

/// Stage 1 → 2: hand the engine's findings to the small-model formatter, or settle a failed or rate-limited run.
fn advance(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, j: Job, t: u64) -> Option<Job> {
    st.running.retain(|r| !(r.branch == j.branch && r.pid == j.child.id()));
    let raw = fs::read_to_string(&j.out).unwrap_or_default();
    let wt = j.worktree.clone()?;
    let abandon = |why: &str| {
        log!("{}: review failed: {why} (raw kept at {})", j.branch, j.out.display());
        let missing = review::missing_sandbox_tools();
        if !missing.is_empty() { log!("{}", review::sandbox_hint(&missing)); }
    };
    match review::parse_engine(&raw, t) {
        review::Engine::Ok { findings, answer, refused } => {
            for r in &refused { log!("{}: /code-review was refused {r}", j.branch); }
            let _ = fs::remove_file(&j.out);
            let ledger = Ledger::load(&ledger_path(repo, &j.branch));
            let files = repo.files_at_head(&j.entry.base, &j.entry.head);
            let prompt = review::format_prompt(&j.branch, &j.entry.base, &j.entry.head, findings.as_deref(), &answer, &files,
                                               &ledger.to_check(), follow_up(&ledger, &j.entry));
            let guidance = fs::read_to_string(wt.join("REVIEW.md")).unwrap_or_default()
                + &j.entry.request.as_ref().map(review::request_block).unwrap_or_default();
            let out = state_dir(repo).join("runs").join(format!("{}-format.json", run_tag(&j.branch, &j.entry.head)));
            if let Some(child) = spawn(review::format_command(&wt, &prompt, &guidance, &cfg.models.small), &out, &j.branch) {
                return Some(Job { kind: Kind::Review, child, out, worktree: Some(wt), refused, ..j });
            }
            abandon("formatter did not start");
        }
        review::Engine::UsageLimit(until) => {
            log!("{}: usage limit, paused until {until}", j.branch);
            st.paused_until = until;
            q.requeue(&j.branch, j.entry.clone(), &|a, b| repo.is_ancestor(a, b));
            cleanup(repo, &wt);
            return None;
        }
        review::Engine::Failed(why) => abandon(&why),
    }
    st.failed += 1;
    q.finish(&j.branch);
    cleanup(repo, &wt);
    None
}

/// CI diagnosis: the medium model at medium effort, read-only and sandboxed like a review, in its own worktree of
/// the head (steer files from the trunk); the prompt carries the failed logs. `finish` removes the worktree.
fn start_ci(repo: &Repo, cfg: &Config, c: CiJob) -> Option<Job> {
    let route = Route { model: cfg.models.medium.clone(), effort: "medium", level: "medium",
                        reason: format!("{} failing CI run(s)", c.failures.len()) };
    let Some(base) = repo.steer_base(&c.head, &cfg.branches.base) else {
        log!("{}: no trunk ({}/{}) to take the diagnosis's guidance from; CI not diagnosed", c.branch, repo.remote,
             cfg.branches.base);
        return None;
    };
    let prompt = review::ci_prompt(&c.branch, &base, &c.head, &c.failures);
    let runs = state_dir(repo).join("runs");
    let wt = runs.join(format!("wt-ci-{}", run_tag(&c.branch, &c.head)));
    if !repo.add_worktree(&wt, &base, &c.head) { log!("{}: could not check out {} for CI diagnosis", c.branch, &c.head[..7]); return None; }
    let _ = fs::create_dir_all(review::session_dir(&PathBuf::from(std::env::var("HOME").unwrap_or_default()), &wt));
    let out = runs.join(format!("{}-ci.json", run_tag(&c.branch, &c.head)));
    let Some(child) = spawn(review::ci_command(&wt, &route, &prompt), &out, &c.branch) else { cleanup(repo, &wt); return None };
    log!("ci diagnosis {} @{} on {}", c.branch, &c.head[..7], route.model);
    let entry = Entry { base: c.head.clone(), head: c.head.clone(), queued_at: 0, model: None, attempts: 0, request: None };
    Some(Job { kind: Kind::Ci, branch: c.branch.clone(), entry, route, child, out, ci: Some(c), started: now(),
               worktree: Some(wt), refused: vec![], fix: None })
}

/// The `watch.sh` event line for a fix: keep the `fix <b>@<sha>: ` prefix stable.
fn fix_line(b: &str, head: &str, r: Result<(&str, &[String]), &str>) -> String {
    match r {
        Ok((sha, ids)) => format!("fix {b}@{}: 🔧 pushed {} → {}", &head[..7], ids.join(" "), &sha[..7]),
        Err(why) => format!("fix {b}@{}: ⛔ not pushed ({why})", &head[..7]),
    }
}

/// A fix checkout's name: the branch with `_` as `__` and every byte outside `[A-Za-z0-9.-]` as `_xx` (hex), plus
/// the short head. Only `[A-Za-z0-9._-]`, since the engine's Edit rule is scoped to its worktree's path and
/// `fix::command` gives a path holding a glob character no rule at all; and one name per branch, since starting a fix
/// removes any leftover at its name (`feat/x` and `feat_x` must not share one).
fn fix_tag(b: &str, head: &str) -> String {
    let mut s = String::new();
    for &x in b.as_bytes() {
        match x {
            b'_' => s += "__",
            x if x.is_ascii_alphanumeric() || x == b'.' || x == b'-' => s.push(x as char),
            x => s += &format!("_{x:02x}"),
        }
    }
    format!("{s}-{}", &head[..7])
}

/// FNV-1a, 64 bits, as 16 hex digits: a short, stable name for a repo.
fn fnv64(b: &[u8]) -> String {
    let h = b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}")
}

/// Where a repo's fix checkouts live: `$XDG_CACHE_HOME` (else `~/.cache`) `/revaiw/<fnv64 of the common dir>/runs`.
/// Not the state dir: Claude Code denies Edit anywhere under a `.git` directory whatever the rule (CLI 2.1.301), so a
/// fix engine working in `<repo>/.git/revaiw/runs/` could never change a thing.
fn fix_root(xdg: Option<&str>, home: &Path, common: &Path) -> PathBuf {
    let cache = xdg.filter(|x| x.starts_with('/')).map(PathBuf::from).unwrap_or_else(|| home.join(".cache"));
    cache.join("revaiw").join(fnv64(common.as_os_str().as_bytes())).join("runs")
}

/// A fix's two checkouts of the head. U is the engine's: untrusted once it has run, so no git ever runs there but its
/// removal. C is the daemon's clean one, which the change is harvested into, checked, committed and pushed from.
fn fix_worktrees(repo: &Repo, b: &str, head: &str) -> (PathBuf, PathBuf) {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let runs = fix_root(std::env::var("XDG_CACHE_HOME").ok().as_deref(), &home, &repo.common);
    let tag = fix_tag(b, head);
    (runs.join(format!("fix-{tag}")), runs.join(format!("fixc-{tag}")))
}

/// Ends whatever a finished child left running in its process group (spawned with `process_group(0)`: pgid = pid).
/// KILL, not TERM: the child itself is gone, and a leftover must not be able to trap the signal.
fn kill_group(pid: u32) {
    let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Fix stage 1: the fix engine edits U, a worktree of the branch's current remote head.
fn start_fix(repo: &Repo, cfg: &Config, b: &str, f: FixJob, head: &str) -> Option<Job> {
    let items = fix::select(&Ledger::load(&ledger_path(repo, b)), &f.ids);
    if items.is_empty() { log!("{b}: fix requested by @{}, but no matching open finding", f.login); return None; }
    let Some(base) = repo.steer_base(head, &cfg.branches.base) else {
        log!("{b}: no trunk ({}/{}) to take the fix engine's guidance from; not fixed", repo.remote, cfg.branches.base);
        return None;
    };
    let (u, _) = fix_worktrees(repo, b, head);
    let _ = fs::create_dir_all(u.parent()?);
    if !repo.add_worktree(&u, &base, head) { log!("{b}: could not check out {} to fix", &head[..7]); return None; }
    if !fix::editable(&u) {
        log!("{b}: the fix checkout {} holds a glob character, so the engine gets no Edit rule and cannot change anything",
             u.display());
    }
    let _ = fs::create_dir_all(review::session_dir(&PathBuf::from(std::env::var("HOME").unwrap_or_default()), &u));
    let out = state_dir(repo).join("runs").join(format!("{}-fix.json", fix_tag(b, head)));
    let cmd = fix::command(&u, &cfg.fix.model, &fix::prompt(b, head, &items));
    let Some(child) = spawn(cmd, &out, b) else { cleanup(repo, &u); return None };
    log!("fix {b} @{} on {} for {} finding(s), requested by @{}", &head[..7], cfg.fix.model, items.len(), f.login);
    let entry = Entry { base, head: head.into(), queued_at: f.queued_at, model: None, attempts: 0, request: None };
    let route = Route { model: cfg.fix.model.clone(), effort: "medium", level: "fix", reason: "requested fix".into() };
    Some(Job { kind: Kind::Fix, branch: b.into(), entry, route, child, out, ci: None, started: now(),
               worktree: Some(u), refused: vec![], fix: Some((f, items, vec![])) })
}

/// Fix stage 2, once the engine has exited: end anything it left running, harvest the findings' files from U into
/// a clean checkout C and guard the change there. Some when the fix now waits for its checks; with none configured
/// it goes straight to the push.
fn fix_checked(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, mut j: Job, t: u64) -> Option<Job> {
    st.running.retain(|r| !(r.branch == j.branch && r.pid == j.child.id()));
    kill_group(j.child.id()); // before the harvest: nothing the engine started may still be changing U
    let (u, c) = fix_worktrees(repo, &j.branch, &j.entry.head);
    match fix::parse_done(&fs::read_to_string(&j.out).unwrap_or_default(), t) {
        fix::FixRun::UsageLimit(until) => {
            log!("{}: usage limit, paused until {until}", j.branch);
            st.paused_until = until;
            // The pause is not the fix's failure: it keeps its attempts and waits for the limit to reset.
            if let Some((f, _, _)) = j.fix.take() { q.defer_fix(&j.branch, f, until); }
            cleanup(repo, &u);
            return None;
        }
        fix::FixRun::Failed(why) => { finish_fix(repo, cfg, q, st, j, Err(format!("the fix run failed: {why}"))); return None; }
        fix::FixRun::Done => {}
    }
    let verdict = {
        let Some((_, items, files)) = j.fix.as_mut() else { return None };
        let mut wanted: Vec<String> = vec![];
        for (_, e) in items.iter() { if !wanted.contains(&e.f.file) { wanted.push(e.f.file.clone()); } }
        if !repo.add_clean_worktree(&c, &j.entry.head) { Err(format!("could not check out {} to commit from", &j.entry.head[..7])) }
        else { fix::harvest(&u, &c, &wanted).and_then(|h| { *files = h; fix_guard(repo, cfg, &c, files, items) }) }
    };
    if verdict.is_err() || cfg.checks.commands.is_empty() { finish_fix(repo, cfg, q, st, j, verdict); return None; }
    Some(j)
}

/// Fix stage 3: the configured checks in C, as a child, once the resource guard allows it.
fn start_checks(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, j: Job) -> Option<Job> {
    let (_, c) = fix_worktrees(repo, &j.branch, &j.entry.head);
    let out = state_dir(repo).join("runs").join(format!("{}-fixchecks.txt", fix_tag(&j.branch, &j.entry.head)));
    match spawn(fix::checks_command(&c, &cfg.checks.commands, cfg.checks.timeout_secs), &out, &j.branch) {
        Some(child) => Some(Job { kind: Kind::FixChecks, child, out, ..j }),
        None => { finish_fix(repo, cfg, q, st, j, Err("the checks did not start".into())); None }
    }
}

/// What the harvested `files` change in C, checked against the findings' files and the line budget. Ok holds the
/// changed paths, which are what gets committed (`changed_in` marks new files intent-to-add, so they can be).
fn fix_guard(repo: &Repo, cfg: &Config, c: &Path, files: &[String], items: &[(String, LedgerEntry)]) -> Result<Vec<String>, String> {
    let changed = repo.changed_in(c, files);
    let allowed: std::collections::HashSet<String> = items.iter().map(|(_, e)| e.f.file.clone()).collect();
    fix::guard(&changed, &allowed, cfg.fix.max_lines)?;
    Ok(changed.into_iter().map(|(_, _, p)| p).collect())
}

/// Fix stage 4, once the checks have exited: end anything they left running, then they pass only when the check
/// script itself exited 0 and every command reported `exit 0`. Then the guard runs again, since a check (`lint --fix`,
/// a formatter) can rewrite files.
fn fix_checks_done(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, j: Job, status: ExitStatus) {
    kill_group(j.child.id()); // before the second guard: nothing a check started may still be changing C
    let ran = if status.success() {
        fix::checks_result(&fs::read_to_string(&j.out).unwrap_or_default(), &cfg.checks.commands)
    } else {
        Err(format!("the check script ended with {status}"))
    };
    let (_, c) = fix_worktrees(repo, &j.branch, &j.entry.head);
    let verdict = match (ran, &j.fix) {
        (Err(e), _) => Err(format!("checks failed: {e}")),
        (Ok(()), Some((_, items, files))) => fix_guard(repo, cfg, &c, files, items),
        (Ok(()), None) => Err("no fix to push".into()),
    };
    finish_fix(repo, cfg, q, st, j, verdict);
}

/// A fix pushes only while `fix.enabled` is on (spec D5): the base's config is re-read every 30 s, so it can have
/// been turned off while this fix ran.
fn while_enabled(enabled: bool, verdict: Result<Vec<String>, String>) -> Result<Vec<String>, String> {
    verdict.and_then(|paths| if enabled { Ok(paths) } else { Err("fixes were turned off while this ran".into()) })
}

/// GitHub refuses a comment over 65,536 characters; a fix over the line budget can carry a patch that long.
const PATCH_MAX: usize = 50_000;

/// The attempted change, for a fix that was not pushed: `git diff` of the fix's files in C. Never git in U.
fn fix_patch(repo: &Repo, c: &Path, files: &[String]) -> String {
    let Some(gitdir) = repo.worktree_gitdir(c).filter(|_| !files.is_empty()) else { return String::new() };
    let diff = crate::git::fix_git(c, &gitdir).args(["diff", "HEAD", "--"]).args(files).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    if diff.chars().count() <= PATCH_MAX { return diff; }
    diff.chars().take(PATCH_MAX).collect::<String>() + "\n… (truncated)\n"
}

/// Fix stage 5: push from C (or say why not), record it in the ledger, comment on the PR, free the branch, and
/// remove both checkouts. `verdict` is Ok with the paths to commit when every guard and check passed.
fn finish_fix(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, j: Job, verdict: Result<Vec<String>, String>) {
    st.running.retain(|r| !(r.branch == j.branch && r.pid == j.child.id()));
    let verdict = while_enabled(cfg.fix.enabled, verdict);
    let (u, c) = fix_worktrees(repo, &j.branch, &j.entry.head);
    if let Some((f, items, files)) = &j.fix {
        let ids: Vec<String> = items.iter().map(|(id, _)| id.clone()).collect();
        let pushed = verdict.and_then(|paths| {
            let gitdir = repo.worktree_gitdir(&c).ok_or("no trusted gitdir for the clean checkout")?;
            fix::commit_and_push(&c, &gitdir, &repo.remote, &j.branch, &paths, &ids, &f.login)
        });
        let lp = ledger_path(repo, &j.branch);
        if let Ok(sha) = &pushed {
            let mut ledger = Ledger::load(&lp);
            ledger.mark_fixed(&ids, &format!("revaiw fix {}", &sha[..7]), sha);
            ledger.save(&lp);
        }
        let line = fix_line(&j.branch, &j.entry.head, pushed.as_ref().map(|s| (s.as_str(), &ids[..])).map_err(String::as_str));
        log!("{line}");
        if github::posting_enabled() {
            if let Some((slug, pr)) = github::repo_slug().zip(github::pr_for(&j.branch)) {
                let patch = if pushed.is_ok() { String::new() } else { fix_patch(repo, &c, files) };
                let failure = pushed.as_ref().err().map(|e| (e.as_str(), patch.as_str()));
                let body = render::fix_comment(&j.branch, &j.entry.head, pushed.as_deref().ok(), items, &f.login, failure);
                if let Err(e) = github::post_comment(&slug, &pr, &body) { log!("{}: fix comment: {e}", j.branch); }
                if pushed.is_ok() {
                    let l = Ledger::load(&lp);
                    let _ = github::sync_pr(&slug, &pr, &l.closed_comment_ids(), &l.open_comment_ids(), true);
                }
            }
        }
    }
    q.finish(&j.branch);
    cleanup(repo, &u);
    cleanup(repo, &c);
}

/// A pinned `revaiw now` asks for a second opinion, so it is never a nits-free follow-up.
fn follow_up(ledger: &Ledger, e: &Entry) -> bool {
    ledger.follow_up() && e.model.is_none()
}

fn ledger_path(repo: &Repo, b: &str) -> PathBuf {
    repo.root.join(".reviews").join(b).join("ledger.json")
}

/// Returns a CI watch for a completed review, so its Actions runs are checked once they finish.
fn finish(repo: &Repo, cfg: &Config, q: &mut Queue, st: &mut Status, ci_due: &mut Vec<CiJob>, mut j: Job, t: u64)
    -> Option<(String, CiWatch)> {
    st.running.retain(|r| !(r.branch == j.branch && r.pid == j.child.id()));
    if let Some(wt) = &j.worktree { cleanup(repo, wt); }
    let raw = fs::read_to_string(&j.out).unwrap_or_default();
    let is_review = j.kind == Kind::Review;
    match review::parse(&raw, t) {
        review::Outcome::Ok(o) => {
            if is_review { q.finish(&j.branch); }
            let _ = fs::remove_file(&j.out);
            let ci = filter::ci_expected_after_lookup(cfg, &j.branch, github::open_pr_base(&j.branch).as_ref().map(|b| b.as_deref()).map_err(|_| ()));
            let mut o = o;
            // A refused tool call is something the engine could not read or check: partial, never ✅.
            if is_review { o.refused = j.refused.clone(); }
            let important = record(repo, cfg, st, &j, &o, t, ci);
            let unread = usize::from(is_review && o.partial());
            if unread > 0 {
                log!("{}: ⚠️ partial review @{}, /code-review {} (see the review comment)", j.branch, &j.entry.head[..7], o.gaps());
            }
            if is_review && !ci {
                if unread > 0 { return None; }
                // No GitHub CI for this head (not on `ci.branches`, no open PR into `ci.pr_bases`): the review is
                // the whole remote verdict. Same wording as the CI-green lines below, so review-watch reads it the same way.
                match important {
                    0 => log!("{}: ✅ ready to merge @{} (review clean; no GitHub CI for this branch)", j.branch, &j.entry.head[..7]),
                    n => log!("{}: review done @{}, {n} important still open", j.branch, &j.entry.head[..7]),
                }
                return None;
            }
            is_review.then(|| (j.branch.clone(), CiWatch { head: j.entry.head.clone(), told_skipped: false, since: t, polled: t,
                                                          important, refused: unread }))
        }
        review::Outcome::UsageLimit(until) => {
            log!("{}: usage limit, paused until {until}", j.branch);
            st.paused_until = until;
            match j.ci.take() {
                Some(c) => ci_due.push(c),
                None => q.requeue(&j.branch, j.entry, &|a, b| repo.is_ancestor(a, b)),
            }
            None
        }
        review::Outcome::Failed(why) => {
            log!("{}: {} failed: {why} (raw kept at {})", j.branch, if is_review { "review" } else { "ci diagnosis" }, j.out.display());
            st.failed += 1;
            if is_review { q.finish(&j.branch); }
            None
        }
    }
}

/// Where a run's time went: push → start (the debounce plus any queue), the CLI run, and the GitHub posting.
/// A CI diagnosis has no push to measure from (queued_at 0), so it shows only the last two.
fn timing(queued_at: u64, started: u64, finished: u64, posted: u64) -> String {
    let review = format!("review {}s · post {}s", finished.saturating_sub(started), posted.saturating_sub(finished));
    if queued_at == 0 { format!("⏱ {review}") } else { format!("⏱ wait {}s · {review}", started.saturating_sub(queued_at)) }
}

/// Returns the branch's open important findings after this run.
fn record(repo: &Repo, cfg: &Config, st: &mut Status, j: &Job, o: &crate::model::ReviewOutput, t: u64, ci: bool) -> usize {
    let lp = ledger_path(repo, &j.branch);
    let mut ledger = Ledger::load(&lp);
    let mut o = o.clone();
    if j.kind == Kind::Review { crate::ledger::converge(follow_up(&ledger, &j.entry), &mut o); }
    if j.kind == Kind::Ci {
        if let Some(base) = repo.range(None, &j.entry.head, &cfg.branches.base) {
            if let Some(changed) = repo.changed_files(&base, &j.entry.head) {
                crate::ledger::demote_outside(&mut o, &changed);
            }
        }
    }
    let o = &o;
    // A push that landed while this ran makes it a review of an old head: a fix it re-reports is parked as
    // `recheck` (not open, so not counted or posted) for the next review to settle.
    let superseded = repo.remote_head(&j.branch)
        .is_some_and(|h| h != j.entry.head && repo.is_ancestor(&j.entry.head, &h));
    let merged = if superseded { ledger.merge_superseded(o, &j.entry.head) } else { ledger.merge(o, &j.entry.head) };
    let title = if j.kind == Kind::Ci { "🧪 CI failures" } else { "🐇 Review" };
    let engine = format!("/code-review {}", j.route.level);
    let body = render::summary(&render::Ctx {
        title, branch: &j.branch, base: &j.entry.base, head: &j.entry.head, model: &j.route.model,
        effort: j.route.effort, reason: &j.route.reason, out: o, ledger: &ledger, fixed_now: &merged.fixed,
        engine: (j.kind == Kind::Review).then_some(engine.as_str()),
    });
    let suffix = if j.kind == Kind::Ci { "-ci" } else { "" };
    let md = lp.with_file_name(format!("{}{suffix}.md", &j.entry.head[..7]));
    let _ = fs::create_dir_all(md.parent().unwrap());
    let _ = fs::write(&md, &body);

    if github::posting_enabled() {
        let target = github::repo_slug().zip(github::pr_for(&j.branch));
        if target.is_none() { log!("{}: no open PR, kept local only", j.branch); }
        if let Some((slug, pr)) = target {
            if let Err(e) = github::post_comment(&slug, &pr, &body) { log!("{}: comment: {e}", j.branch); }
            if j.kind == Kind::Review {
                let open = ledger.open();
                let n = |s: &str| open.iter().filter(|(_, e)| e.f.severity == s).count();
                if let Some((state, text)) = github::status_text(n("important"), n("nit"), usize::from(o.partial())) {
                    github::set_status(&slug, &j.entry.head, state, &text);
                }
            }
            let added = repo.added_lines(&format!("{}/{}", repo.remote, pr.base), &j.entry.head);
            // Only this run's findings: their lines refer to `head`. Older ones carry older line numbers.
            let this_run: std::collections::HashSet<String> = o.findings.iter().map(crate::ledger::finding_id).collect();
            let items: Vec<_> = ledger.open().into_iter()
                .filter(|(id, e)| e.comment_id.is_none() && this_run.contains(id))
                .map(|(id, e)| (id, e.f)).collect();
            if let Some((payload, ids)) = github::review_payload(&j.entry.head, &items, &added) {
                match github::post_review(&slug, &pr, &payload) {
                    Ok(cids) => {
                        for (id, cid) in ids.iter().zip(cids) {
                            if let Some(e) = ledger.findings.get_mut(id) { e.comment_id = Some(cid); }
                        }
                        ledger.save(&lp); // persist before anything else can fail, so nothing is posted twice
                    }
                    Err(e) => log!("{}: inline review: {e}", j.branch),
                }
            }
            // The PR follows the ledger: threads of findings closed there (by this review, an agent or a person)
            // are resolved, and superseded revaiw comments are hidden as outdated.
            match github::sync_pr(&slug, &pr, &ledger.closed_comment_ids(), &ledger.open_comment_ids(), true) {
                Ok(s) => for e in s.errors { log!("{}: sync PR: {e}", j.branch); },
                Err(e) => log!("{}: sync PR: {e}", j.branch),
            }
        }
    }
    ledger.save(&lp);
    let tm = timing(j.entry.queued_at, j.started, t, now());

    let open = ledger.open();
    let n = |s: &str| open.iter().filter(|(_, e)| e.f.severity == s).count();
    let model = if j.kind == Kind::Ci { format!("ci:{}", j.route.model) } else { j.route.model.clone() };
    let d = Done { branch: j.branch.clone(), head: j.entry.head.clone(), model,
                   effort: j.route.effort.into(), reason: j.route.reason.clone(), important: n("important"),
                   nits: n("nit"), pre_existing: n("pre_existing"), md: md.display().to_string(), finished_at: t };
    // These lines are the event stream for `watch.sh` (Claude's Monitor tool): keep the prefixes stable.
    let line = match j.kind {
        Kind::Ci => format!("ci {}@{}: 🧪 {} important, {} pre-existing · {tm} → {}", d.branch, &d.head[..7], d.important, d.pre_existing, d.md),
        Kind::Review if d.important > 0 => format!("review {}@{}: 🔴 {} important, {} nits · {tm} → {}", d.branch, &d.head[..7], d.important, d.nits, d.md),
        Kind::Review if o.partial() => format!("review {}@{}: ⚠️ partial (0 important, {} nits; {}) · {tm} → {}",
                                               d.branch, &d.head[..7], d.nits, o.gaps(), d.md),
        // An engine job never reaches here: `advance` hands it to the formatter, whose output is recorded. Nor does a
        // checks job (it hands over to the engine) or a fix job (`finish_fix` reports it).
        Kind::Review | Kind::Checks | Kind::Engine | Kind::Fix | Kind::FixChecks => format!("review {}@{}: clean (0 important, {} nits){} · {tm} → {}", d.branch, &d.head[..7], d.nits,
                                if ci { ", waiting for CI" } else { "" }, d.md),
    };
    log!("{line}");
    if notify_enabled(&state_dir(repo)) {
        let _ = std::process::Command::new("notify-send").args(["Review done", &line])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    let important = d.important;
    st.push_done(d);
    important
}


#[cfg(test)]
mod tests {
    #[test]
    fn the_github_repo_is_looked_up_lazily_and_retried() {
        let mut s = super::Slug::default();
        let calls = std::cell::Cell::new(0);
        let fail = |e: &'static str| { let c = &calls; move || { c.set(c.get() + 1); Err::<String, String>(e.into()) } };
        assert_eq!(s.refresh(100, fail("no network")).as_deref(),
                   Some("inbox: no GitHub repo slug yet: no network; retrying"));
        assert!(s.live.is_none());
        assert_eq!(s.refresh(110, fail("no network")), None, "not before the retry interval");
        assert_eq!(calls.get(), 1);
        assert_eq!(s.refresh(160, fail("no network")), None, "the same failure is logged once");
        assert_eq!(calls.get(), 2);
        assert!(s.refresh(220, fail("not logged in")).is_some(), "a different failure is logged");
        assert_eq!(s.refresh(280, || Ok("o/r".to_string())), None);
        assert_eq!(s.live.as_ref().map(|l| l.slug.as_str()), Some("o/r"));
        assert_eq!(s.refresh(999, fail("x")), None, "known: never looked up again");
        assert_eq!(calls.get(), 3);
    }
    #[test]
    fn a_held_review_is_logged_once_per_hold() {
        let g = crate::allocate::Guard { avail: 2.5, load1: 14.25, cores: 16.0, heavy: 3 };
        let mut holding = false;
        assert_eq!(super::hold_line(&mut holding, true, &g).as_deref(),
                   Some("holding: resource guard (avail 2.5 GiB, load 14.25, heavy 3)"));
        assert_eq!(super::hold_line(&mut holding, true, &g), None, "not again on the next tick");
        assert_eq!(super::hold_line(&mut holding, false, &g), None);
        assert!(super::hold_line(&mut holding, true, &g).is_some(), "a new hold is logged");
    }
    #[test]
    fn an_invalid_config_keeps_the_running_daemons_last_good_one() {
        use crate::config::{Config, Loaded};
        let mut good = Config::default();
        good.fix.enabled = true;
        let loaded = |cfg: Config, err: Option<&str>, trunk_ok| Loaded { cfg, err: err.map(String::from), trunk_ok };
        // Valid file: it applies.
        assert_eq!(super::next_config(&Config::default(), loaded(good.clone(), None, true)), (good.clone(), None));
        // Invalid TOML on a readable trunk: `load` hands back defaults, which must not replace the running config.
        let (cfg, err) = super::next_config(&good, loaded(Config::default(), Some("bad toml"), true));
        assert_eq!(cfg, good);
        assert_eq!(err.as_deref(), Some("bad toml"));
        // Unreadable trunk: same.
        assert_eq!(super::next_config(&good, loaded(Config::default(), Some("no HEAD"), false)).0, good);
        // File deleted on a readable trunk: no error, defaults are the repo's.
        assert_eq!(super::next_config(&good, loaded(Config::default(), None, true)).0, Config::default());
    }
    #[test]
    fn notifications_are_off_until_the_marker_exists() {
        let d = tempfile::tempdir().unwrap();
        assert!(!super::notify_enabled(d.path()));
        std::fs::write(d.path().join("notify"), "").unwrap();
        assert!(super::notify_enabled(d.path()));
    }
    #[test]
    fn the_resource_guard_is_off_until_the_marker_exists() {
        let d = tempfile::tempdir().unwrap();
        assert!(!super::guard_enabled(d.path()));
        std::fs::write(d.path().join("guard"), "").unwrap();
        assert!(super::guard_enabled(d.path()));
    }
    #[test]
    fn a_pinned_review_is_never_a_follow_up() {
        use crate::{ledger::Ledger, model::{Finding, ReviewOutput}, queue::Entry};
        let mut l = Ledger::default();
        let f = Finding { severity: "important".into(), file: "a.ts".into(), line: 1, end_line: None, summary: "x".into(),
                          why: "y".into(), snippet: String::new(), lang: String::new(), suggested_patch: None,
                          suggestion: None, on_base: false };
        l.merge(&ReviewOutput { findings: vec![f], ..Default::default() }, "s");
        let e = Entry { base: "a".into(), head: "b".into(), queued_at: 0, model: None, attempts: 0, request: None };
        assert!(super::follow_up(&l, &e));
        assert!(!super::follow_up(&l, &Entry { model: Some("sonnet".into()), ..e }));
    }
    #[test]
    fn requests_are_taken_by_rename_so_late_appends_survive() {
        let d = tempfile::tempdir().unwrap();
        let req = d.path().join("now-requests");
        std::fs::write(&req, "feat/a\n").unwrap();
        assert_eq!(super::take_requests(&req).as_deref(), Some("feat/a\n"));
        assert!(!req.exists(), "taken");
        std::fs::write(&req, "feat/b\n").unwrap(); // an append after the take lands in a fresh file
        assert_eq!(super::take_requests(&req).as_deref(), Some("feat/b\n"));
        assert!(super::take_requests(&req).is_none());
    }
    #[test]
    fn timing_splits_wait_review_and_post() {
        assert_eq!(super::timing(1000, 1092, 1163, 1166), "⏱ wait 92s · review 71s · post 3s");
        // A CI diagnosis has no push-to-start wait to report (queued_at 0).
        assert_eq!(super::timing(0, 1092, 1163, 1166), "⏱ review 71s · post 3s");
    }
    #[test]
    fn fix_event_lines_keep_their_prefix() {
        assert_eq!(super::fix_line("feat/x", "abc1234aaa", Ok(("def5678bbb", &["0123456789ab".to_string()][..]))),
                   "fix feat/x@abc1234: 🔧 pushed 0123456789ab → def5678");
        assert_eq!(super::fix_line("feat/x", "abc1234aaa", Err("push rejected: x")),
                   "fix feat/x@abc1234: ⛔ not pushed (push rejected: x)");
    }
    #[test]
    fn fix_worktree_names_hold_only_safe_characters_and_never_collide() {
        let tag = |b| super::fix_tag(b, "abc1234aaa");
        assert_eq!(tag("feat/x"), "feat_2fx-abc1234");
        assert_eq!(tag("release-1.2"), "release-1.2-abc1234");
        // `feat_x` beside `feat/x`: one name each, or removing one's checkout deletes the other's live one.
        let names = ["feat/x", "feat_x", "feat__x", "feat_2fx", "a[b]*{c}?\\é d"].map(tag);
        assert_eq!(names.iter().collect::<std::collections::HashSet<_>>().len(), names.len(), "{names:?}");
        // Glob characters, `\`, spaces and non-ASCII would make the engine's scoped Edit rule unusable.
        assert_eq!(names[4], "a_5bb_5d_2a_7bc_7d_3f_5c_c3_a9_20d-abc1234");
        for t in &names { assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)), "{t}"); }
    }
    #[test]
    fn fix_checkouts_live_outside_the_git_dir() {
        // Claude Code refuses Edit under any `.git` path, so the fix engine's worktree cannot live in the state dir.
        assert_eq!(super::fnv64(b""), "cbf29ce484222325");
        assert_eq!(super::fnv64(b"a"), "af63dc4c8601ec8c");
        let (home, common) = (std::path::Path::new("/home/u"), std::path::Path::new("/r/.git"));
        let id = super::fnv64(b"/r/.git");
        assert_eq!(super::fix_root(None, home, common), home.join(".cache/revaiw").join(&id).join("runs"));
        assert_eq!(super::fix_root(Some("/xdg"), home, common), std::path::PathBuf::from(format!("/xdg/revaiw/{id}/runs")));
        // The XDG spec: an empty or relative value is ignored.
        assert_eq!(super::fix_root(Some(""), home, common), super::fix_root(None, home, common));
        assert_eq!(super::fix_root(Some("rel"), home, common), super::fix_root(None, home, common));
        assert_ne!(super::fix_root(None, home, std::path::Path::new("/s/.git")), super::fix_root(None, home, common));
    }
    #[test]
    fn a_fix_pushes_only_while_fixes_are_on() {
        assert_eq!(super::while_enabled(true, Ok(vec!["a.ts".into()])), Ok(vec!["a.ts".to_string()]));
        assert_eq!(super::while_enabled(false, Ok(vec!["a.ts".into()])), Err("fixes were turned off while this ran".to_string()));
        assert_eq!(super::while_enabled(false, Err("x".into())), Err("x".to_string()));
    }
    #[test]
    fn state_and_marker_are_named_revaiw() {
        let repo = crate::git::Repo { root: "/r".into(), common: "/r/.git".into(), remote: "origin".into() };
        assert_eq!(super::state_dir(&repo), std::path::PathBuf::from("/r/.git/revaiw"));
        assert_eq!(crate::render::MARKER, "<!-- revaiw -->");
    }
}
