//! Local review server: reviews every pushed feature branch with a slim headless `claude -p`.
//! @see README.md
mod allocate;
mod cli;
mod config;
mod daemon;
mod filter;
mod fix;
mod git;
mod github;
mod inbox;
mod ledger;
mod mention;
mod model;
mod queue;
mod render;
mod report;
mod review;
mod route;
mod status;

use std::fs;
use std::process::{Command, Stdio};

/// A pid is this repo's daemon only when it runs `__daemon` (its own argument, not a substring) from the repo's root:
/// a recorded pid can be reused by anything, another repo's daemon included.
fn is_our_daemon(cmdline: &[u8], cwd: Option<&std::path::Path>, root: &std::path::Path) -> bool {
    cmdline.split(|&b| b == 0).any(|a| a == b"__daemon") && cwd == Some(root)
}

fn alive(pid: u32, root: &std::path::Path) -> bool {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok().and_then(|p| p.canonicalize().ok());
    is_our_daemon(&cmdline, cwd.as_deref(), root)
}

/// Starts `cmd` in a session of its own (setsid): no controlling tty, so nothing the daemon runs can open
/// `/dev/tty` to prompt, and its process group is its pid (as `process_group(0)` gave, which setsid replaces: a group
/// leader cannot call setsid).
fn detach(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe, the only call made between fork and exec.
    unsafe { cmd.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }) }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("status");
    let Some(repo) = std::env::current_dir().ok().and_then(|d| git::Repo::discover(&d)) else {
        eprintln!("revaiw: run this inside a git repository that has a remote (e.g. origin). \
                   Commands: start | stop | status | report | now | sweep | watch | init | notify | guard");
        std::process::exit(2);
    };
    let dir = daemon::state_dir(&repo);
    fs::create_dir_all(&dir).unwrap();
    let pidf = dir.join("daemon.pid");
    let pid = fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse::<u32>().ok()).filter(|p| alive(*p, &repo.root));

    match cmd {
        "__daemon" => daemon::run(repo),
        "init" => {
            for (p, wrote) in cli::init(&repo.root) {
                println!("{} {}", if wrote { "wrote" } else { "kept " }, p.display());
            }
            if let Err(e) = repo.exclude_reviews() { eprintln!("could not add /.reviews/ to .git/info/exclude: {e}"); }
            if let config::Loaded { trunk_ok: false, err: Some(e), .. } = config::load(&repo) { eprintln!("{e}"); }
            for (tool, ok, hint) in cli::prereqs() {
                println!("{} {tool}{}", if ok { "✓" } else { "✗" }, if ok { String::new() } else { format!(": {hint}") });
            }
            if let Some(h) = std::env::current_exe().ok().and_then(|e| cli::path_hint(&e, &std::env::var("PATH").unwrap_or_default())) {
                println!("✗ PATH: {h}");
            }
            println!("next: commit .review/config.toml and REVIEW.md to the default branch, then `revaiw start`");
        }
        "watch" => {
            let branch = args.get(2).cloned().or_else(|| repo.git(&["branch", "--show-current"])).unwrap_or_default();
            let status = Command::new("bash").arg("-c").arg(include_str!("../watch.sh")).arg("watch.sh").arg(&branch)
                .current_dir(&repo.root).status();
            std::process::exit(status.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
        }
        "start" => {
            let _ = fs::remove_file(dir.join("stopped"));
            let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
            let want_no_post = std::env::var("REVAIW_NO_POST").as_deref() == Ok("1");
            if let Some(p) = pid {
                let same_bin = fs::read_link(format!("/proc/{p}/exe")).ok() == me;
                let same_mode = dir.join("no-post").exists() == want_no_post;
                if same_bin && same_mode { println!("revaiw already running (pid {p})"); return; }
                println!("revaiw: restarting pid {p} ({})", if same_bin { "posting mode changed" } else { "a different build" });
                let _ = Command::new("kill").arg(p.to_string()).status();
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            if want_no_post { let _ = fs::write(dir.join("no-post"), "1"); } else { let _ = fs::remove_file(dir.join("no-post")); }
            let missing = review::missing_sandbox_tools();
            if !missing.is_empty() { eprintln!("{}", review::sandbox_hint(&missing)); std::process::exit(1); }
            let log = dir.join("daemon.log");
            if fs::metadata(&log).map(|m| m.len() > 1_000_000).unwrap_or(false) {
                let _ = fs::rename(&log, dir.join("daemon.log.1"));
            }
            let f = fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
            let child = detach(Command::new(std::env::current_exe().unwrap()).arg("__daemon"))
                .current_dir(&repo.root)
                .stdin(Stdio::null()).stdout(f.try_clone().unwrap()).stderr(f).spawn().unwrap();
            fs::write(&pidf, child.id().to_string()).unwrap();
            println!("revaiw started (pid {}) · log {}", child.id(), log.display());
        }
        "stop" => {
            let _ = fs::write(dir.join("stopped"), "");
            let st = status::Status::load(&dir.join("status.json"));
            for r in &st.running {
                let _ = Command::new("kill").args(["-TERM", "--", &format!("-{}", r.pid)]).status();
            }
            match pid {
                Some(p) => {
                    let _ = Command::new("kill").arg(p.to_string()).status();
                    let _ = fs::remove_file(&pidf);
                    println!("revaiw stopped (pid {p}, {} running reviews cancelled)", st.running.len());
                }
                None => println!("revaiw not running"),
            }
        }
        "sweep" => {
            // Bring open PRs' (or the named branches') threads and comments in line with their ledgers. A dry run
            // unless --apply: it changes GitHub state.
            let Some(slug) = github::repo_slug() else { eprintln!("no GitHub remote"); std::process::exit(2) };
            let apply = args[2..].iter().any(|a| a == "--apply");
            let want: Vec<String> = args[2..].iter().filter(|a| *a != "--apply").cloned().collect();
            let prs = github::open_prs().unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1) });
            let ledger_of = |b: &str| repo.root.join(".reviews").join(b).join("ledger.json");
            let (go, skipped) = github::sweep_targets(prs, &want, &|b| ledger_of(b).exists());
            for s in skipped { println!("skipped {s}"); }
            for (b, pr) in go {
                let ledger = ledger::Ledger::load(&ledger_of(&b));
                match github::sync_pr(&slug, &pr, &ledger.closed_comment_ids(), &ledger.open_comment_ids(), apply) {
                    Ok(s) => {
                        println!("#{} {b}: {} {} threads resolved, {} reopened, {} comments hidden{}", pr.number,
                                 if apply { "" } else { "would have" }, s.resolved, s.reopened, s.hidden,
                                 if s.errors.is_empty() { String::new() } else { format!(", {} failed", s.errors.len()) });
                        for e in s.errors { println!("    {e}"); }
                    }
                    Err(e) => println!("#{} {b}: {e}", pr.number),
                }
            }
            if !apply { println!("dry run: revaiw sweep --apply [branch...] makes these changes"); }
        }
        "report" => {
            let hours = report::hours_arg(&args[2..]).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(2) });
            // The rotated log first, so a window that spans the rotation reads in order.
            let log: String = ["daemon.log.1", "daemon.log"].iter()
                .map(|f| fs::read_to_string(dir.join(f)).unwrap_or_default()).collect();
            print!("{}", report::render(&report::summarize(&log, daemon::now().saturating_sub(hours * 3600)), hours));
            println!("  log: {}", dir.join("daemon.log").display());
        }
        "notify" => {
            let marker = dir.join("notify");
            match cli::notify_arg(&args[2..]).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(2) }) {
                Some(true) => { fs::write(&marker, "").unwrap(); println!("desktop notifications on"); }
                Some(false) => { let _ = fs::remove_file(&marker); println!("desktop notifications off"); }
                None => println!("desktop notifications {}", if daemon::notify_enabled(&dir) { "on" } else { "off" }),
            }
        }
        "guard" => {
            let marker = dir.join("guard");
            match cli::guard_arg(&args[2..]).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(2) }) {
                Some(true) => { fs::write(&marker, "").unwrap(); println!("resource guard on"); }
                Some(false) => { let _ = fs::remove_file(&marker); println!("resource guard off"); }
                None => println!("resource guard {}", if daemon::guard_enabled(&dir) { "on" } else { "off" }),
            }
        }
        "now" => {
            let (tier, mut branches) = route::now_args(&args[2..]).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(2) });
            if branches.is_empty() { branches.push(repo.git(&["branch", "--show-current"]).expect("no branch")); }
            use std::io::Write;
            let mut f = fs::OpenOptions::new().create(true).append(true).open(dir.join("now-requests")).unwrap();
            f.write_all(route::now_body(&branches, tier.as_deref()).as_bytes()).unwrap();
            for b in &branches {
                println!("queued {b}{}{}", tier.as_deref().map(|t| format!(" on {t}")).unwrap_or_default(),
                         if pid.is_none() { " (daemon not running: start it)" } else { "" });
            }
        }
        _ => {
            let st = status::Status::load(&dir.join("status.json"));
            let q = queue::Queue::read(&dir.join("queue.json"));
            match pid { Some(p) => println!("revaiw running (pid {p})"), None => println!("revaiw stopped") }
            if st.paused_until > daemon::now() { println!("paused (usage limit) until epoch {}", st.paused_until); }
            for r in &st.running { println!("  ▶ {} @{} on {}", r.branch, &r.head[..7], r.model); }
            for (b, e) in &q.entries { println!("  … {} @{} queued", b, &e.head[..7]); }
            println!("notifications: {}", if daemon::notify_enabled(&dir) { "on" } else { "off" });
            println!("resource guard: {}", if daemon::guard_enabled(&dir) { "on" } else { "off" });
            println!("  ✓ {} today · ✗ {} failed", st.done_today, st.failed);
            for d in st.completed.iter().rev().take(5) {
                println!("    {} @{}{} {}i {}n → {}", d.branch, &d.head[..7], d.pr.map(|n| format!(" #{n}")).unwrap_or_default(),
                         d.important, d.nits, d.md);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pid_is_ours_only_when_it_is_this_repos_daemon() {
        let root = std::path::Path::new("/r");
        let ours = b"/home/u/.cargo/bin/revaiw\0__daemon\0";
        assert!(super::is_our_daemon(ours, Some(root), root));
        // A reused pid: another program, revaiw's CLI, or another repo's daemon.
        assert!(!super::is_our_daemon(b"vim\0notes\0", Some(root), root));
        assert!(!super::is_our_daemon(b"/usr/bin/revaiw\0status\0", Some(root), root));
        assert!(!super::is_our_daemon(b"revaiw\0x__daemon\0", Some(root), root), "the whole argument");
        assert!(!super::is_our_daemon(ours, Some(std::path::Path::new("/other")), root));
        assert!(!super::is_our_daemon(ours, None, root), "cwd unreadable: not provably ours");
    }
    #[test]
    fn the_daemon_runs_in_a_session_of_its_own() {
        let mut child = super::detach(&mut std::process::Command::new("sleep").arg("5")).spawn().unwrap();
        let pid = child.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // Fields after the `(comm)`: state ppid pgrp session tty_nr ...
        let f: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!((f[2], f[3], f[4]), (pid.to_string().as_str(), pid.to_string().as_str(), "0"), "pgrp, session, no tty");
    }
}
