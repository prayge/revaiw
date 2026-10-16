//! Fix jobs: a headless, sandboxed `claude -p` edits the files of the requested findings; revaiw then checks the
//! change (only those files, within the line budget, the configured checks pass) and pushes it to the branch,
//! never over a newer push. Only on request (`@review fix` or 👍) and only with `fix.enabled` (spec D5).
//! @see README.md
use crate::ledger::{Ledger, LedgerEntry};
use std::collections::HashSet;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

const FIX: &str = include_str!("../fix.md");
/// Plus the worktree-scoped Edit rule `command` adds.
const FIX_ALLOWED: &[&str] = &["Read", "Grep", "Glob", "Bash(git diff:*)", "Bash(git show:*)",
    "Bash(git status:*)", "Bash(git log:*)"];

pub enum FixRun { Done, UsageLimit(u64), Failed(String) }

/// The requested open findings: every open important one when `ids` is empty, else the named ones still open.
/// Never one in a steer file: the fix checkout holds the base's copy of those, and they are never committed.
pub fn select(l: &Ledger, ids: &[String]) -> Vec<(String, LedgerEntry)> {
    l.open().into_iter()
        .filter(|(id, e)| if ids.is_empty() { e.f.severity == "important" } else { ids.contains(id) })
        .filter(|(_, e)| !crate::git::is_steer(&e.f.file))
        .collect()
}

pub fn prompt(branch: &str, head: &str, items: &[(String, LedgerEntry)]) -> String {
    let list: String = items.iter().map(|(id, e)| {
        let patch = e.f.suggested_patch.as_deref()
            .map(|p| format!("\n  Suggested patch:\n{}\n{p}\n{}", crate::render::fence(p), crate::render::fence(p)))
            .unwrap_or_default();
        format!("- {id} `{}:{}` {}\n  Why: {}{patch}\n", e.f.file, e.f.line, e.f.summary, e.f.why)
    }).collect();
    format!("Branch `{branch}` at {head}. Fix these findings, each in its own file. Change nothing else.\n\n{list}")
}

/// The same sandbox as the review engine (no network, writes confined to the checkout, fail closed), with edits
/// allowed inside the worktree only and no command that commits or pushes. A bare `Edit` would allow writes
/// anywhere (the sandbox confines Bash, not the file tools); `Edit(//<abs>/**)` is an absolute-path rule, and an
/// Edit rule covers the Write tool too (`Write(path)` rules never match). The worktree's `.git` is denied (a deny
/// wins), but that only stops the Edit tool: sandboxed Bash can still write `wt/.git`. The real protection is that
/// the daemon never runs git in the engine's worktree (`harvest` copies the finding files out with plain file I/O).
/// A path holding glob characters (or `\`) cannot be written as a rule that means only itself, so it gets no Edit
/// rule at all: the engine cannot edit and the guard sees nothing. Nor can it edit anywhere under a `.git` directory
/// (Claude Code 2.1.301 denies that whatever the rule), so `wt` never lives in the git dir.
pub fn command(wt: &Path, model: &str, prompt: &str) -> Command {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut c = crate::review::headless(wt, model, &crate::review::engine_settings(&home));
    c.arg("--add-dir").arg(wt).arg("--add-dir").arg(crate::review::session_dir(&home, wt));
    let root = wt.canonicalize().unwrap_or(wt.to_path_buf()).display().to_string();
    c.args(["-p", prompt, "--allowedTools"]).args(FIX_ALLOWED);
    if editable(wt) { c.arg(format!("Edit(/{root}/**)")); }
    c.args(["--disallowedTools", "NotebookEdit", "WebFetch", "WebSearch", "Read(**/.env*)", "Grep(**/.env*)",
            "Edit(**/.env*)"])
        .arg(format!("Edit(/{root}/.git)")).arg(format!("Edit(/{root}/.git/**)"))
        .args(["--append-system-prompt", FIX]);
    c
}

/// Whether `command` can give the engine an Edit rule for `wt`: not when its canonical path holds a glob character
/// (or `\`), since no rule would then mean only that path.
pub fn editable(wt: &Path) -> bool {
    !wt.canonicalize().unwrap_or(wt.to_path_buf()).to_string_lossy().contains(['*', '?', '[', ']', '{', '}', '\\'])
}

/// Every changed file must be one a selected finding names, and the change must fit `max_lines`.
pub fn guard(changed: &[(u32, u32, String)], allowed: &HashSet<String>, max_lines: u32) -> Result<(), String> {
    if changed.is_empty() { return Err("the fix changed nothing".into()); }
    if let Some((_, _, p)) = changed.iter().find(|(_, _, p)| !allowed.contains(p)) {
        return Err(format!("the fix changed {p}, which no selected finding names"));
    }
    let lines: u32 = changed.iter().map(|(a, d, _)| a + d).sum();
    if lines > max_lines { return Err(format!("the fix is {lines} lines, over fix.max_lines ({max_lines})")); }
    Ok(())
}

/// Runs every configured check in the worktree, each under `timeout`, outside the engine sandbox (they may need the
/// network, like a package install). Spawned as a child so the daemon loop never blocks on it. `--foreground` keeps
/// each check in the child's process group (plain `timeout` gives it one of its own), so the daemon can kill
/// whatever a check left running once the child exits.
pub fn checks_command(wt: &Path, cmds: &[String], secs: u64) -> Command {
    let script = format!("for c in \"$@\"; do printf '### %s\\n' \"$c\"; timeout --foreground -k 15 {secs} sh -c \"$c\" 2>&1; \
                          printf 'exit %d\\n' $?; done");
    let mut c = Command::new("nice");
    c.current_dir(wt).args(["-n", "10", "sh", "-c", &script, "sh"]).args(cmds);
    c
}

/// Ok only when every one of `cmds` reported `exit 0`; else each failure, with the last 40 lines of its output.
/// A block opens only where `### ` starts a line. A block that names no command (a check's own output opened a
/// line with `### `) cannot be read and fails, so no check can fake a result; a command with no block did not run
/// (the shell died first). The exit line follows the output directly, so it may share the output's last line.
pub fn checks_result(output: &str, cmds: &[String]) -> Result<(), String> {
    let starts: Vec<usize> = output.match_indices("### ").map(|(i, _)| i)
        .filter(|&i| i == 0 || output.as_bytes()[i - 1] == b'\n').collect();
    let (mut failed, mut ran) = (vec![], HashSet::new());
    for (k, &i) in starts.iter().enumerate() {
        let block = &output[i + 4..starts.get(k + 1).copied().unwrap_or(output.len())];
        let Some((cmd, rest)) = cmds.iter().find_map(|c| {
            let r = block.strip_prefix(c.as_str())?;
            if r.is_empty() { Some((c, r)) } else { r.strip_prefix('\n').map(|r| (c, r)) }
        }) else {
            failed.push(format!("a check printed `### {}` at the start of a line, so the results cannot be read",
                                block.lines().next().unwrap_or("")));
            continue;
        };
        ran.insert(cmd.as_str());
        let lines: Vec<&str> = rest.lines().collect();
        if !lines.last().is_some_and(|l| l.ends_with("exit 0")) {
            let tail = lines[lines.len().saturating_sub(41)..].join("\n");
            failed.push(format!("`{cmd}` failed:\n{tail}"));
        }
    }
    failed.extend(cmds.iter().filter(|c| !ran.contains(c.as_str())).map(|c| format!("`{c}` did not run")));
    if failed.is_empty() { Ok(()) } else { Err(failed.join("\n\n")) }
}

/// Every git call on the fix worktree is `git::fix_git` (the trusted gitdir, no fsmonitor or hooks, literal paths),
/// bounded by `timeout -k 5 <secs>` and unable to prompt: no terminal prompt, ssh in batch mode, stdin closed. The
/// daemon has no one to answer a credential or passphrase prompt, and a hung push would hold its branch forever.
fn git_command(wt: &Path, gitdir: &Path, secs: u64, args: &[&str]) -> Command {
    let inner = crate::git::fix_git(wt, gitdir);
    let mut c = Command::new("timeout");
    c.args(["-k", "5", &secs.to_string()]).arg(inner.get_program()).args(inner.get_args()).args(args)
        .env("GIT_TERMINAL_PROMPT", "0").env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .stdin(std::process::Stdio::null());
    c
}

fn git(wt: &Path, gitdir: &Path, secs: u64, args: &[&str]) -> Result<String, String> {
    let o = git_command(wt, gitdir, secs, args).output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()) }
    else if o.status.code() == Some(124) || o.status.code() == Some(137) { Err(format!("timed out after {secs}s")) }
    else { Err(String::from_utf8_lossy(&o.stderr).chars().take(400).collect()) }
}

/// In the clean checkout C (never the engine's worktree): commits only `files` with the user's git identity, then
/// pushes HEAD to the branch. Never a path that is not `plain`, never a steer file (those always come from the
/// base), and never an empty list: `git commit --` with no paths commits the whole index (a worktree from
/// `add_worktree` has the base's steer files staged). The push leases the branch at the commit the fix started
/// from, so it lands only while the branch is still exactly there: a newer push, a force push back or a deleted
/// branch all reject it.
/// `gitdir` is C's trusted admin dir (`Repo::worktree_gitdir`), never taken from the worktree itself.
pub fn commit_and_push(wt: &Path, gitdir: &Path, remote: &str, branch: &str, files: &[String], ids: &[String], login: &str)
    -> Result<String, String> {
    if files.is_empty() { return Err("nothing to commit".into()); }
    if let Some(p) = files.iter().find(|p| !plain(p)) {
        return Err(format!("refusing to commit {p}: not a plain relative path"));
    }
    if let Some(p) = files.iter().find(|p| crate::git::is_steer(p)) {
        return Err(format!("refusing to commit {p}: reviewer guidance files come from the base"));
    }
    let lease = git(wt, gitdir, 120, &["rev-parse", "HEAD"])?;
    let msg = format!("revaiw: fix {}\n\nRequested-by: {login}", if ids.is_empty() { "open findings".to_string() } else { ids.join(" ") });
    let mut args = vec!["commit", "-q", "-m", msg.as_str(), "--"];
    args.extend(files.iter().map(String::as_str));
    git(wt, gitdir, 120, &args)?;
    let sha = git(wt, gitdir, 120, &["rev-parse", "HEAD"])?;
    let dest = format!("refs/heads/{branch}");
    git(wt, gitdir, 300, &["push", "-q", &format!("--force-with-lease={dest}:{lease}"), remote, &format!("HEAD:{dest}")])
        .map_err(|e| format!("push rejected: {e}"))?;
    Ok(sha)
}

/// Carries the fix out of the engine's worktree `untrusted` (U) into the daemon's clean checkout `clean` (C): only
/// `files` (the selected findings' files), as bytes, with plain file I/O. No git ever runs in U, where the engine
/// could have planted a `.git`, a submodule's filter driver or an fsmonitor that git would run outside the sandbox;
/// and anything else the engine changed in U is never read, so non-finding edits are dropped by construction.
/// A file gone from U is deleted in C. The whole harvest is refused, before anything is written, for a path that is
/// not plain (`plain`) or is a steer file, for a symlink anywhere on the path in U (it could read a file from
/// outside into the commit) or in C (it could carry the write outside), for anything in U that is not a regular
/// file, and for a file over 1 MiB on either side. U is read through fds (`untrusted_file`), so a process the engine
/// left behind cannot swap a link in mid-read. Returns the files it changed in C, in input order.
/// On Err, C may be partly written (a write or delete failed after others succeeded): the caller must discard C.
pub fn harvest(untrusted: &Path, clean: &Path, files: &[String]) -> Result<Vec<String>, String> {
    let mut plan = vec![];
    for f in files {
        if !plain(f) || crate::git::is_steer(f) {
            return Err(format!("refusing to harvest {f}"));
        }
        let (new, old) = (untrusted_file(untrusted, f)?, regular_file(clean, f)?);
        if new != old { plan.push((f.clone(), new)); }
    }
    for (f, new) in &plan {
        let target = clean.join(f);
        match new {
            Some(bytes) => {
                if let Some(dir) = target.parent() { std::fs::create_dir_all(dir).map_err(|e| format!("{f}: {e}"))?; }
                std::fs::write(&target, bytes).map_err(|e| format!("{f}: {e}"))?;
            }
            None => std::fs::remove_file(&target).map_err(|e| format!("{f}: {e}"))?,
        }
    }
    Ok(plan.into_iter().map(|(f, _)| f).collect())
}

/// The most harvest reads of one file, on either side: finding files are source, and a sparse multi-GB file must
/// never be read into memory.
const MAX_BYTES: u64 = 1 << 20;

/// A path git and the filesystem both read as written: normal components joined by single `/`, none of them `.`,
/// `..` or `.git` in any case. Git normalises `./x`, `a//b` and `a/`, so such a path could reach the commit as a
/// steer file or a `.git` no check saw. (`Path::components` normalises them too, so it cannot be the test.)
fn plain(p: &str) -> bool {
    p.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.eq_ignore_ascii_case(".git"))
}

/// `openat(at, name, flags)`, or `open(name, flags)` with no `at`, as an fd that closes on drop.
fn open_at(at: Option<&OwnedFd>, name: &[u8], flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let name = std::ffi::CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    let at = at.map_or(libc::AT_FDCWD, |fd| fd.as_raw_fd());
    // SAFETY: `name` is NUL-terminated and outlives the call; `at` is AT_FDCWD or an fd we hold open.
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 { return Err(std::io::Error::last_os_error()); }
    // SAFETY: openat just returned this fd open, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `root/f`'s bytes from the engine's worktree U (`f` is `plain`), None when it is gone. Race-free against a process
/// still changing U: each component is opened with `O_NOFOLLOW` relative to the fd of the one before, and the bytes
/// come from the fd that was checked, so no path is resolved twice. A symlink anywhere (ELOOP), anything but a
/// regular file, or more than `MAX_BYTES` is refused.
fn untrusted_file(root: &Path, f: &str) -> Result<Option<Vec<u8>>, String> {
    const DIR: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // Non-blocking, so opening a FIFO never waits for a writer (fstat refuses it next); no effect on a regular file.
    const FILE: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let refuse = |e: std::io::Error| format!("refusing to harvest {f}: {e}");
    let step = |at: &OwnedFd, name: &str, flags| match open_at(Some(at), name.as_bytes(), flags) {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None), // deleted
        Err(e) => Err(refuse(e)),
    };
    let mut at = open_at(None, root.as_os_str().as_bytes(), DIR).map_err(refuse)?;
    let (dirs, name) = f.rsplit_once('/').unwrap_or(("", f));
    for part in dirs.split('/').filter(|p| !p.is_empty()) {
        let Some(next) = step(&at, part, DIR)? else { return Ok(None) };
        at = next;
    }
    let Some(fd) = step(&at, name, FILE)? else { return Ok(None) };
    let file = std::fs::File::from(fd);
    let meta = file.metadata().map_err(refuse)?; // fstat on the open fd
    if !meta.is_file() { return Err(format!("refusing to harvest {f}: not a regular file")); }
    if meta.len() > MAX_BYTES { return Err(format!("{f} is larger than 1 MiB")); }
    let mut bytes = vec![];
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(refuse)?;
    if bytes.len() as u64 > MAX_BYTES { return Err(format!("{f} is larger than 1 MiB")); } // grew after the fstat
    Ok(Some(bytes))
}

/// `root/f`'s bytes in the daemon's clean checkout C, None when it does not exist; Err when it, or any directory on
/// the way to it, is a symlink (a PR can commit one), it is not a regular file, or it is over `MAX_BYTES`.
fn regular_file(root: &Path, f: &str) -> Result<Option<Vec<u8>>, String> {
    let mut p = root.to_path_buf();
    for c in Path::new(f).components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("refusing to harvest {f}: {} is a symlink", p.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{f}: {e}")),
        }
    }
    let meta = std::fs::symlink_metadata(&p).map_err(|e| format!("{f}: {e}"))?;
    if !meta.is_file() { return Err(format!("refusing to harvest {f}: not a regular file")); }
    if meta.len() > MAX_BYTES { return Err(format!("{f} is larger than 1 MiB")); }
    std::fs::read(&p).map(Some).map_err(|e| format!("{f}: {e}"))
}

pub fn parse_done(stdout: &str, now: u64) -> FixRun {
    let v: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => return FixRun::Failed(format!("bad json: {e}")),
    };
    if v["is_error"].as_bool().unwrap_or(false) {
        return match crate::review::usage_limit(&v, now) {
            Some(t) => FixRun::UsageLimit(t),
            None => FixRun::Failed(v["result"].as_str().unwrap_or("").chars().take(300).collect()),
        };
    }
    FixRun::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Finding, ReviewOutput};
    use std::collections::HashSet;
    use std::process::Command;

    fn finding(file: &str, summary: &str, sev: &str) -> Finding {
        Finding { severity: sev.into(), file: file.into(), line: 3, end_line: None, summary: summary.into(),
                  why: "a retry double-charges".into(), snippet: String::new(), lang: String::new(),
                  suggested_patch: Some("--- a/x\n+++ b/x\n".into()), suggestion: None, on_base: false }
    }
    fn ledger() -> (Ledger, Vec<String>) {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![finding("a.ts", "x", "important"), finding("b.ts", "y", "nit")],
                                ..Default::default() }, "s");
        let ids = l.findings.keys().cloned().collect();
        (l, ids)
    }
    #[test] fn select_takes_open_importants_or_the_named_open_ids() {
        let (mut l, _) = ledger();
        let all = select(&l, &[]);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.f.file, "a.ts");
        let nit = l.findings.iter().find(|(_, e)| e.f.severity == "nit").unwrap().0.clone();
        assert_eq!(select(&l, &[nit.clone()]).len(), 1, "a named nit is fixed too");
        l.dismiss(&nit, "no");
        assert!(select(&l, &[nit]).is_empty(), "never a settled one");
    }
    #[test] fn select_skips_steer_files() {
        let mut l = Ledger::default();
        l.merge(&ReviewOutput { findings: vec![finding("a.ts", "x", "important"),
                                               finding(".claude/settings.json", "y", "important"),
                                               finding("pkg/CLAUDE.md", "z", "important")],
                                ..Default::default() }, "s");
        let all = select(&l, &[]);
        assert_eq!(all.iter().map(|(_, e)| e.f.file.as_str()).collect::<Vec<_>>(), vec!["a.ts"]);
        let steer: Vec<String> = l.findings.iter().filter(|(_, e)| e.f.file != "a.ts").map(|(id, _)| id.clone()).collect();
        assert!(select(&l, &steer).is_empty(), "a named steer finding is never fixed");
    }
    #[test] fn the_prompt_names_each_finding_and_forbids_anything_else() {
        let (l, _) = ledger();
        let p = prompt("feat/x", "abc1234", &select(&l, &[]));
        assert!(p.contains("a.ts:3") && p.contains("a retry double-charges") && p.contains("--- a/x"), "{p}");
        assert!(p.contains("Change nothing else"));
    }
    #[test] fn the_fix_engine_may_edit_but_never_push_or_reach_the_network() {
        let c = command(std::path::Path::new("/wt"), "m", "P");
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let allowed = args.iter().skip_while(|a| *a != "--allowedTools").take_while(|a| *a != "--disallowedTools")
            .cloned().collect::<Vec<_>>();
        // Edits confined to the worktree (`//` is an absolute path; an Edit rule covers Write too), never bare.
        assert!(allowed.contains(&"Edit(//wt/**)".to_string()), "{allowed:?}");
        assert!(!allowed.iter().any(|a| a == "Edit" || a == "Write" || a.starts_with("Write(")), "{allowed:?}");
        assert!(!args.iter().any(|a| a.starts_with("Write(")), "Write(path) rules never match: {args:?}");
        assert!(!allowed.iter().any(|a| a.contains("push") || a.contains("commit")), "{allowed:?}");
        // The worktree's `.git` pointer is never editable (a deny wins over the allow).
        let denied = args.iter().skip_while(|a| *a != "--disallowedTools").collect::<Vec<_>>();
        assert!(denied.contains(&&"Edit(//wt/.git)".to_string()) && denied.contains(&&"Edit(//wt/.git/**)".to_string()),
                "{denied:?}");
        let settings = args.iter().find(|a| a.contains("\"sandbox\"")).unwrap();
        assert!(settings.contains("\"failIfUnavailable\":true") && settings.contains("Bash(git push *)"));
    }
    #[test] fn a_worktree_path_with_glob_characters_gets_no_edit_rule() {
        for wt in ["/w*t", "/w?t", "/w[t]", "/w{t,u}", "/w\\t"] {
            let c = command(std::path::Path::new(wt), "m", "P");
            let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            let allowed = args.iter().skip_while(|a| *a != "--allowedTools").take_while(|a| *a != "--disallowedTools")
                .collect::<Vec<_>>();
            assert!(!allowed.iter().any(|a| a.starts_with("Edit") || a.starts_with("Write")), "{wt}: {allowed:?}");
        }
    }
    #[test] fn the_guard_allows_only_finding_files_within_the_budget() {
        let allowed: HashSet<String> = ["a.ts".to_string()].into();
        assert_eq!(guard(&[(3, 1, "a.ts".into())], &allowed, 200), Ok(()));
        assert!(guard(&[], &allowed, 200).unwrap_err().contains("changed nothing"));
        assert!(guard(&[(1, 0, "a.ts".into()), (1, 0, "c.ts".into())], &allowed, 200).unwrap_err().contains("c.ts"));
        assert!(guard(&[(150, 60, "a.ts".into())], &allowed, 200).unwrap_err().contains("210 lines"));
    }
    #[test] fn checks_run_every_command_and_report_the_failures() {
        let d = tempfile::tempdir().unwrap();
        let cmds: Vec<String> = vec!["echo ok".into(), "echo bad >&2; exit 3".into()];
        let out = checks_command(d.path(), &cmds, 30).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let e = checks_result(&text, &cmds).unwrap_err();
        assert!(e.contains("echo bad >&2; exit 3") && e.contains("exit 3") && e.contains("bad"), "{e}");
        assert!(!e.contains("`echo ok`"), "a passing check is not reported: {e}");
        let cmds: Vec<String> = vec!["true".into(), "printf ok".into()]; // output without a final newline passes too
        let ok = checks_command(d.path(), &cmds, 30).output().unwrap();
        assert_eq!(checks_result(&String::from_utf8_lossy(&ok.stdout), &cmds), Ok(()));
    }
    #[test] fn every_check_stays_in_the_checks_process_group() {
        // The daemon kills the checks child's group once it exits; a check's leftover must be in it. Plain `timeout`
        // moves each check into a group of its own, out of that reach.
        use std::os::unix::process::CommandExt;
        let d = tempfile::tempdir().unwrap();
        let bg = d.path().join("bg");
        let cmds = vec![format!("sleep 30 & echo $! > '{}'", bg.display())];
        let mut child = checks_command(d.path(), &cmds, 30).process_group(0)
            .stdout(std::process::Stdio::null()).spawn().unwrap();
        child.wait().unwrap();
        let pid = std::fs::read_to_string(&bg).unwrap().trim().to_string();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let _ = Command::new("kill").args(["-KILL", &pid]).status();
        let pgrp = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(2).unwrap().to_string();
        assert_eq!(pgrp, child.id().to_string());
        // A check over its time limit is still stopped, and fails.
        let slow = vec!["sleep 5".to_string()];
        let out = checks_command(d.path(), &slow, 1).output().unwrap();
        assert!(checks_result(&String::from_utf8_lossy(&out.stdout), &slow).unwrap_err().contains("exit 124"));
    }
    #[test] fn missing_or_garbled_check_output_fails() {
        let one = vec!["true".to_string()];
        assert!(checks_result("", &one).unwrap_err().contains("`true` did not run"));
        // A check whose own output opens a line with `### ` cannot be read, so it fails closed, even when the fake
        // block it prints ends `exit 0` and it then exits non-zero.
        let d = tempfile::tempdir().unwrap();
        let cmds = vec!["printf '### fake\\nexit 0\\n'".to_string()];
        let out = checks_command(d.path(), &cmds, 30).output().unwrap();
        let e = checks_result(&String::from_utf8_lossy(&out.stdout), &cmds).unwrap_err();
        assert!(e.contains("### fake"), "{e}");
        let cmds = vec!["printf 'exit 0\\n### fake\\n'; exit 1".to_string()];
        let out = checks_command(d.path(), &cmds, 30).output().unwrap();
        assert!(checks_result(&String::from_utf8_lossy(&out.stdout), &cmds).is_err());
        // A shell killed after the first check: the second never reported.
        let two = vec!["true".to_string(), "make test".to_string()];
        assert!(checks_result("### true\nexit 0\n", &two).unwrap_err().contains("`make test` did not run"));
    }

    fn git(d: &std::path::Path, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(d).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    /// A bare remote with branch `feat/x`, and a clone of it at `feat/x` to fix in.
    fn remote_and_clone() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (bare, wt) = (t.path().join("remote.git"), t.path().join("wt"));
        git(t.path(), &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(t.path(), &["clone", "-q", bare.to_str().unwrap(), wt.to_str().unwrap()]);
        std::fs::write(wt.join("a.ts"), "1\n").unwrap();
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::write(wt.join(".claude/settings.json"), "{}\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "base"]);
        git(&wt, &["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        (t, bare, wt)
    }
    #[test] fn commit_and_push_are_bounded_and_never_prompt() {
        let (wt, gd) = (std::path::Path::new("/c"), std::path::Path::new("/r/.git/worktrees/c"));
        let c = git_command(wt, gd, 300, &["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(c.get_program(), "timeout");
        assert_eq!(args[..4], ["-k", "5", "300", "git"]);
        // Still `fix_git`'s hardened call: trusted gitdir, no fsmonitor or hooks, literal paths.
        let inner = crate::git::fix_git(wt, gd);
        let want: Vec<String> = inner.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[4..4 + want.len()], want[..]);
        assert_eq!(args[4 + want.len()..], ["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        let env: Vec<(String, String)> = c.get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()).unwrap_or_default())).collect();
        assert!(env.contains(&("GIT_TERMINAL_PROMPT".into(), "0".into())), "{env:?}");
        assert!(env.contains(&("GIT_SSH_COMMAND".into(), "ssh -oBatchMode=yes".into())), "{env:?}");
        let commit = git_command(wt, gd, 120, &["commit"]);
        assert_eq!(commit.get_args().take(3).map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["-k", "5", "120"]);
    }
    #[test] fn commit_takes_only_the_named_files() {
        let (_t, bare, wt) = remote_and_clone();
        std::fs::write(wt.join("a.ts"), "2\n").unwrap();
        std::fs::write(wt.join(".claude/settings.json"), "{\"base\":1}\n").unwrap(); // the swapped-in base file
        git(&wt, &["add", ".claude/settings.json"]); // staged, as `add_worktree`'s checkout_from leaves it
        let sha = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["a.ts".into()], &["0123456789ab".into()], "kim").unwrap();
        let files = git(&wt, &["show", "--name-only", "--format=", &sha]);
        assert_eq!(files, "a.ts");
        let msg = git(&wt, &["log", "-1", "--format=%B", &sha]);
        assert!(msg.starts_with("revaiw: fix 0123456789ab") && msg.contains("Requested-by: kim"), "{msg}");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), sha);
    }
    #[test] fn a_branch_that_moved_rejects_the_push() {
        let (t, bare, wt) = remote_and_clone();
        let other = t.path().join("other");
        git(t.path(), &["clone", "-q", "-b", "feat/x", bare.to_str().unwrap(), other.to_str().unwrap()]);
        std::fs::write(other.join("b.ts"), "agent\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-qm", "agent push"]);
        git(&other, &["push", "-q", "origin", "feat/x"]);
        let moved = git(&bare, &["rev-parse", "refs/heads/feat/x"]);
        std::fs::write(wt.join("a.ts"), "2\n").unwrap();
        let e = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["a.ts".into()], &[], "kim").unwrap_err();
        assert!(e.contains("push rejected"), "{e}");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), moved, "the newer push is untouched");
    }
    fn exists(bare: &std::path::Path, r: &str) -> bool {
        Command::new("git").arg("-C").arg(bare).args(["rev-parse", "--verify", "--quiet", r])
            .stdout(std::process::Stdio::null()).status().unwrap().success()
    }
    #[test] fn a_branch_forced_back_rejects_the_push() {
        let (_t, bare, wt) = remote_and_clone();
        let older = git(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("b.ts"), "agent\n").unwrap();
        git(&wt, &["add", "b.ts"]);
        git(&wt, &["commit", "-qm", "agent work"]);
        git(&wt, &["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        // The author drops that commit with a force push; the fix worktree still sits on it.
        git(&bare, &["update-ref", "refs/heads/feat/x", &older]);
        std::fs::write(wt.join("a.ts"), "2\n").unwrap();
        let e = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["a.ts".into()], &[], "kim").unwrap_err();
        assert!(e.contains("push rejected"), "{e}");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), older, "the dropped commit stays dropped");
    }
    #[test] fn a_deleted_branch_is_not_recreated() {
        let (_t, bare, wt) = remote_and_clone();
        git(&bare, &["update-ref", "-d", "refs/heads/feat/x"]);
        std::fs::write(wt.join("a.ts"), "2\n").unwrap();
        let e = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["a.ts".into()], &[], "kim").unwrap_err();
        assert!(e.contains("push rejected"), "{e}");
        assert!(!exists(&bare, "refs/heads/feat/x"), "the deleted branch stays deleted");
    }
    #[test] fn commit_refuses_empty_and_steer_files() {
        let (_t, bare, wt) = remote_and_clone();
        let before = git(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("a.ts"), "2\n").unwrap();
        std::fs::write(wt.join(".claude/settings.json"), "{\"base\":1}\n").unwrap();
        git(&wt, &["add", ".claude/settings.json"]); // staged, as after add_worktree
        assert_eq!(commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &[], &[], "kim"), Err("nothing to commit".into()));
        let e = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["a.ts".into(), ".claude/settings.json".into()], &[], "kim")
            .unwrap_err();
        assert_eq!(e, "refusing to commit .claude/settings.json: reviewer guidance files come from the base");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), before, "nothing committed");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), before, "nothing pushed");
    }
    #[test] fn commit_treats_paths_literally() {
        let (_t, _bare, wt) = remote_and_clone();
        std::fs::write(wt.join("[C]LAUDE.md"), "pr\n").unwrap();
        std::fs::write(wt.join("CLAUDE.md"), "base\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "both"]);
        git(&wt, &["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        std::fs::write(wt.join("[C]LAUDE.md"), "fixed\n").unwrap();
        std::fs::write(wt.join("CLAUDE.md"), "swapped in\n").unwrap();
        git(&wt, &["add", "CLAUDE.md"]); // staged, as the base swap leaves it
        let sha = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &["[C]LAUDE.md".into()], &[], "kim").unwrap();
        assert_eq!(git(&wt, &["show", "--name-only", "--format=", &sha]), "[C]LAUDE.md", "a glob never widens the commit");
    }
    /// A linked fix worktree whose `.git` pointer the engine rewrote to a gitdir of its own making, with an
    /// fsmonitor and hooks that would run on the daemon's unsandboxed git calls.
    #[test] fn a_tampered_dot_git_is_not_trusted() {
        let (t, bare, main) = remote_and_clone();
        let lw = t.path().join("lw");
        git(&main, &["worktree", "add", "-q", "--detach", lw.to_str().unwrap(), "HEAD"]);
        let marker = t.path().join("pwned");
        let script = t.path().join("evil.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let fake = lw.join("fake");
        git(t.path(), &["init", "-q", fake.to_str().unwrap()]);
        git(&fake, &["commit", "-q", "--allow-empty", "-m", "x"]);
        // A shell command rather than the script: git runs it through `sh`, so no freshly written file can fail
        // with ETXTBSY while another test thread forks (the hooks below are best effort for that reason).
        git(&fake, &["config", "core.fsmonitor", &format!("touch '{}';:", marker.display())]);
        for hook in ["pre-commit", "post-commit", "pre-push"] {
            std::fs::copy(&script, fake.join(".git/hooks").join(hook)).unwrap();
        }
        std::fs::write(lw.join(".git"), format!("gitdir: {}\n", fake.join(".git").display())).unwrap();
        std::fs::write(lw.join("a.ts"), "2\n").unwrap();

        let repo = crate::git::Repo::discover(&main).unwrap();
        let changed = repo.changed_in(&lw, &["a.ts".into()]);
        assert!(!marker.exists(), "changed_in ran the planted fsmonitor");
        assert!(changed.iter().any(|(_, _, p)| p == "a.ts"), "read through the real gitdir: {changed:?}");
        let gitdir = repo.worktree_gitdir(&lw).expect("found from the common dir's side");
        assert_eq!(gitdir.canonicalize().unwrap(), main.join(".git/worktrees/lw").canonicalize().unwrap());
        let sha = commit_and_push(&lw, &gitdir, "origin", "feat/x", &["a.ts".into()], &[], "kim").unwrap();
        assert!(!marker.exists(), "commit_and_push ran a planted fsmonitor or hook");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), sha, "committed and pushed through the real gitdir");
    }
    fn write(root: &std::path::Path, f: &str, body: &str) {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    fn read(root: &std::path::Path, f: &str) -> String { std::fs::read_to_string(root.join(f)).unwrap() }
    #[test] fn harvest_copies_only_finding_files() {
        let (u, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for f in ["a.ts", "b.ts", "c.ts"] { write(c.path(), f, "1\n"); }
        write(u.path(), "a.ts", "2\n");
        write(u.path(), "b.ts", "2\n");
        write(u.path(), "c.ts", "1\n"); // unchanged: not reported
        write(u.path(), "sub/.git/config", "[filter \"ev\"]\n\tclean = touch x\n");
        assert_eq!(harvest(u.path(), c.path(), &["a.ts".into(), "c.ts".into()]), Ok(vec!["a.ts".to_string()]));
        assert_eq!(read(c.path(), "a.ts"), "2\n");
        assert_eq!(read(c.path(), "b.ts"), "1\n", "a non-finding edit never leaves the engine's worktree");
        assert!(!c.path().join("sub").exists());
        write(u.path(), "new/d.ts", "x\n"); // a new file, in a new directory
        assert_eq!(harvest(u.path(), c.path(), &["new/d.ts".into()]), Ok(vec!["new/d.ts".to_string()]));
        assert_eq!(read(c.path(), "new/d.ts"), "x\n");
    }
    #[test] fn harvest_refuses_symlinks_and_dot_git_and_escape() {
        let (u, c, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(c.path(), "a.ts", "1\n");
        write(out.path(), "secret", "key\n");
        std::os::unix::fs::symlink(out.path().join("secret"), u.path().join("a.ts")).unwrap();
        assert!(harvest(u.path(), c.path(), &["a.ts".into()]).is_err(), "a symlink is not a file");
        assert_eq!(read(c.path(), "a.ts"), "1\n");
        for f in [".git/config", "../x", "sub/.git/config", "/etc/passwd", "CLAUDE.md", ".claude/settings.json"] {
            write(u.path(), "ok.ts", "y\n");
            assert!(harvest(u.path(), c.path(), &["ok.ts".into(), f.into()]).is_err(), "{f}");
            assert!(!c.path().join("ok.ts").exists(), "{f}: refused as a whole, nothing written");
        }
        // A symlinked directory in the clean checkout would carry the write outside it.
        std::os::unix::fs::symlink(out.path(), c.path().join("src")).unwrap();
        write(u.path(), "src/secret", "overwritten\n");
        assert!(harvest(u.path(), c.path(), &["src/secret".into()]).is_err());
        assert_eq!(read(out.path(), "secret"), "key\n");
        // A symlinked directory in the engine's worktree would read a file from outside it into the commit.
        std::os::unix::fs::symlink(out.path(), u.path().join("cfg")).unwrap();
        assert!(harvest(u.path(), c.path(), &["cfg/secret".into()]).is_err());
        assert!(!c.path().join("cfg").exists());
        // The clean checkout's own file a symlink (a PR can commit one): never written through.
        std::os::unix::fs::symlink(out.path().join("secret"), c.path().join("l.ts")).unwrap();
        write(u.path(), "l.ts", "overwritten\n");
        assert!(harvest(u.path(), c.path(), &["l.ts".into()]).is_err());
        assert_eq!(read(out.path(), "secret"), "key\n");
    }
    #[test] fn harvest_refuses_files_over_1_mib() {
        let (u, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sparse = |root: &std::path::Path, f: &str, len: u64| {
            std::fs::File::create(root.join(f)).unwrap().set_len(len).unwrap(); // sparse: no disk used
        };
        write(c.path(), "a.ts", "1\n");
        sparse(u.path(), "a.ts", 10 << 20);
        assert_eq!(harvest(u.path(), c.path(), &["a.ts".into()]), Err("a.ts is larger than 1 MiB".into()));
        assert_eq!(read(c.path(), "a.ts"), "1\n");
        write(u.path(), "b.ts", "2\n");
        sparse(c.path(), "b.ts", 10 << 20);
        assert_eq!(harvest(u.path(), c.path(), &["b.ts".into()]), Err("b.ts is larger than 1 MiB".into()));
        sparse(u.path(), "e.ts", 1 << 20);
        assert_eq!(harvest(u.path(), c.path(), &["e.ts".into()]), Ok(vec!["e.ts".to_string()]), "1 MiB itself fits");
    }
    /// A process the engine left behind (on macOS possibly in its own session) can swap a directory on the path for
    /// a symlink while harvest runs. Harvest opens every component with O_NOFOLLOW and reads the fd it opened, so no
    /// timing lets it read through a link: the outside file never reaches C.
    #[test] fn harvest_reads_through_fds_not_paths() {
        use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
        let (u, c, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(out.path(), "x.ts", "secret\n");
        write(c.path(), "d/x.ts", "1\n");
        std::os::unix::fs::symlink(out.path(), u.path().join("d")).unwrap();
        assert!(harvest(u.path(), c.path(), &["d/x.ts".into()]).unwrap_err().contains("d/x.ts"), "a linked directory");
        // The swap, racing the harvest: `d` flips between a real directory and a link to `out`.
        std::fs::remove_file(u.path().join("d")).unwrap();
        write(u.path(), "real/x.ts", "2\n");
        let stop = Arc::new(AtomicBool::new(false));
        let flipper = std::thread::spawn({
            let (u, out, stop) = (u.path().to_path_buf(), out.path().to_path_buf(), stop.clone());
            move || {
                let (d, real) = (u.join("d"), u.join("real"));
                while !stop.load(Ordering::Relaxed) {
                    std::fs::rename(&real, &d).unwrap();
                    std::fs::rename(&d, &real).unwrap();
                    std::os::unix::fs::symlink(&out, &d).unwrap();
                    std::fs::remove_file(&d).unwrap();
                }
            }
        });
        let leaked = (0..3000).any(|_| {
            let _ = harvest(u.path(), c.path(), &["d/x.ts".into()]);
            std::fs::read_to_string(c.path().join("d/x.ts")).is_ok_and(|s| s == "secret\n")
        });
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert!(!leaked, "harvest read a file from outside U through a swapped-in symlink");
        // A FIFO is opened without blocking (or the daemon would wait for a writer forever), then refused.
        let fifo = u.path().join("p.ts");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let (tx, rx) = std::sync::mpsc::channel();
        let (uu, cc) = (u.path().to_path_buf(), c.path().to_path_buf());
        std::thread::spawn(move || tx.send(harvest(&uu, &cc, &["p.ts".into()])));
        assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).expect("harvest blocked on a FIFO").is_err());
    }
    /// Git normalises `./x`, `a//b` and `a/`, so such a path would reach the commit as a steer file or a `.git` the
    /// checks never saw. Only normal components joined by single `/` pass, and `.git` in any case is refused.
    #[test] fn only_plain_paths_are_harvested_or_committed() {
        let bad = ["./.review/config.toml", "sub/.GIT/config", ".Git/config", "./a.ts", "a/./b.ts", "a//b.ts", "a.ts/", ""];
        let (u, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for f in [".review/config.toml", "a.ts", "a/b.ts", "sub/.GIT/config", ".Git/config"] { write(u.path(), f, "x\n"); }
        for f in bad {
            write(u.path(), "ok.ts", "y\n");
            assert!(harvest(u.path(), c.path(), &["ok.ts".into(), f.into()]).is_err(), "{f}");
            assert!(!c.path().join("ok.ts").exists(), "{f}: refused as a whole, nothing written");
        }
        let (_t, bare, wt) = remote_and_clone();
        let before = git(&wt, &["rev-parse", "HEAD"]);
        write(&wt, ".review/config.toml", "evil\n");
        git(&wt, &["add", "-N", ".review/config.toml"]); // as `changed_in` leaves a harvested new file
        for f in bad {
            let e = commit_and_push(&wt, &wt.join(".git"), "origin", "feat/x", &[f.into()], &[], "kim").unwrap_err();
            assert!(e.starts_with(&format!("refusing to commit {f}")), "{f}: {e}");
        }
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), before, "nothing committed");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), before, "nothing pushed");
    }
    #[test] fn harvest_handles_deletion() {
        let (u, c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(c.path(), "a.ts", "1\n");
        assert_eq!(harvest(u.path(), c.path(), &["a.ts".into(), "never.ts".into()]), Ok(vec!["a.ts".to_string()]));
        assert!(!c.path().join("a.ts").exists());
    }
    /// The reviewer's attack, set up: the head has a gitlink `sub`; U is its fix worktree, and the engine's work in
    /// U is the fix to `a.ts` plus a plant: `sub/.git` (with `x` in its index) whose filter driver's clean command
    /// touches the marker, and a same-size edit to `x` with a new mtime, so git must run the filter to compare it.
    /// Git run in U spawns `git status` in `sub`, which runs the filter outside the sandbox.
    /// Returns (temp dir, bare remote, main clone, repo, head, U, marker).
    fn planted() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf, crate::git::Repo, String,
                     std::path::PathBuf, std::path::PathBuf) {
        let (t, bare, main) = remote_and_clone();
        let subsrc = t.path().join("subsrc");
        git(t.path(), &["init", "-q", subsrc.to_str().unwrap()]);
        git(&subsrc, &["commit", "-q", "--allow-empty", "-m", "s"]);
        let subsha = git(&subsrc, &["rev-parse", "HEAD"]);
        git(&main, &["update-index", "--add", "--cacheinfo", &format!("160000,{subsha},sub")]);
        git(&main, &["commit", "-qm", "gitlink"]);
        git(&main, &["push", "-q", "origin", "HEAD:refs/heads/feat/x"]);
        let head = git(&main, &["rev-parse", "HEAD"]);
        let repo = crate::git::Repo::discover(&main).unwrap();
        let u = t.path().join("u");
        assert!(repo.add_worktree(&u, &head, &head));
        let (sub, marker) = (u.join("sub"), t.path().join("pwned"));
        std::fs::remove_dir(&sub).unwrap(); // the empty gitlink directory
        git(t.path(), &["clone", "-q", subsrc.to_str().unwrap(), sub.to_str().unwrap()]);
        write(&sub, "x", "data\n");
        git(&sub, &["add", "x"]);
        write(&sub, ".gitattributes", "* filter=ev\n");
        git(&sub, &["config", "filter.ev.clean", &format!("touch '{}'; cat", marker.display())]);
        git(&sub, &["config", "core.fsmonitor", &format!("touch '{}';:", marker.display())]);
        write(&sub, "x", "dat2\n");
        std::fs::File::options().write(true).open(sub.join("x")).unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(86_400)).unwrap();
        write(&u, "a.ts", "2\n");
        assert!(!marker.exists(), "set up without running the plant");
        (t, bare, main, repo, head, u, marker)
    }
    /// The full fix flow after the engine: harvest U into the clean checkout C, measure, commit and push in C,
    /// remove both. No git ever runs in U, so the plant never runs.
    #[test] fn no_git_ever_runs_in_the_engine_worktree() {
        let (t, bare, main, repo, head, u, marker) = planted();
        let c = t.path().join("c");
        assert!(repo.add_clean_worktree(&c, &head));
        let files = harvest(&u, &c, &["a.ts".into()]).unwrap();
        assert_eq!(files, vec!["a.ts".to_string()]);
        assert_eq!(repo.changed_in(&c, &files), vec![(1, 1, "a.ts".to_string())]);
        let gitdir = repo.worktree_gitdir(&c).unwrap();
        let sha = commit_and_push(&c, &gitdir, "origin", "feat/x", &files, &[], "kim").unwrap();
        assert_eq!(git(&main, &["show", "--name-only", "--format=", &sha]), "a.ts", "only the finding file");
        assert_eq!(git(&bare, &["rev-parse", "refs/heads/feat/x"]), sha);
        repo.remove_worktree(&u);
        repo.remove_worktree(&c);
        assert!(!marker.exists(), "a planted filter or fsmonitor ran");
    }
    /// The control for the test above: the plant is live. Even round 2's hardened git (trusted gitdir, no
    /// fsmonitor or hooks) runs it when pointed at U, because filter drivers come from the submodule's own config.
    #[test] fn the_planted_filter_fires_when_git_runs_in_the_engine_worktree() {
        let (_t, _bare, _main, repo, _head, u, marker) = planted();
        let gitdir = repo.worktree_gitdir(&u).unwrap();
        let _ = crate::git::fix_git(&u, &gitdir).args(["diff", "--numstat", "-z", "HEAD"]).output().unwrap();
        assert!(marker.exists(), "the plant is live: git in U runs it");
    }
    #[test] fn a_finished_fix_run_is_parsed() {
        assert!(matches!(parse_done(r#"{"is_error":false,"result":"done"}"#, 0), FixRun::Done));
        assert!(matches!(parse_done(r#"{"is_error":true,"result":"usage limit reached|1792065600"}"#, 0),
                         FixRun::UsageLimit(1_792_065_600)));
        assert!(matches!(parse_done("garbage", 0), FixRun::Failed(_)));
    }
}
