//! `revaiw init` and the prerequisite check behind `/revaiw-init`.
use std::path::{Path, PathBuf};

const CONFIG: &str = include_str!("../../templates/config.toml");
const REVIEW: &str = include_str!("../../templates/REVIEW.md");

/// Writes `.review/config.toml` and `REVIEW.md` where absent. Never overwrites.
pub fn init(root: &Path) -> Vec<(PathBuf, bool)> {
    [(root.join(crate::config::PATH), CONFIG), (root.join("REVIEW.md"), REVIEW)].into_iter().map(|(p, body)| {
        let wrote = !p.exists() && p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
            && std::fs::write(&p, body).is_ok();
        (p, wrote)
    }).collect()
}

fn on_path(name: &str) -> bool {
    std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join(name).is_file())
}

/// Each tool revaiw needs: present or not, and how to get it.
pub fn prereqs() -> Vec<(&'static str, bool, &'static str)> {
    let mut v = vec![
        ("claude", on_path("claude") || crate::review::claude_bin().is_file(), "https://docs.claude.com/claude-code, then log in once"),
        ("gh", on_path("gh"), "https://cli.github.com, then `gh auth login` (repo scope)"),
        ("git", on_path("git"), "your package manager"),
    ];
    if cfg!(target_os = "linux") {
        v.push(("bwrap", on_path("bwrap"), "sudo apt install bubblewrap socat (dnf/pacman alike)"));
        v.push(("socat", on_path("socat"), "sudo apt install bubblewrap socat (dnf/pacman alike)"));
    }
    v.push(("graphify", on_path("graphify"), "optional: lets the reviewer query a code graph"));
    v
}

/// When the folder `cargo install` put revaiw in (usually `~/.cargo/bin`) is not on `PATH`, how to add it. Without
/// it the shell says "revaiw: command not found" after a successful install.
pub fn path_hint(exe: &Path, path_var: &str) -> Option<String> {
    let dir = exe.parent()?;
    let on = path_var.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d) == dir);
    (!on).then(|| format!("revaiw is installed in {0}, which is not on your PATH, so your shell won't find `revaiw`. \
                           Add it: echo 'export PATH=\"{0}:$PATH\"' >> ~/.bashrc && source ~/.bashrc", dir.display()))
}

/// `revaiw notify [on|off]`: `None` reports the state, `Some(b)` sets it.
pub fn notify_arg(args: &[String]) -> Result<Option<bool>, String> { toggle_arg("notify", args) }

/// `revaiw guard [on|off]`: the same, for the resource guard.
pub fn guard_arg(args: &[String]) -> Result<Option<bool>, String> { toggle_arg("guard", args) }

fn toggle_arg(name: &str, args: &[String]) -> Result<Option<bool>, String> {
    match args {
        [] => Ok(None),
        [a] if a == "on" => Ok(Some(true)),
        [a] if a == "off" => Ok(Some(false)),
        _ => Err(format!("usage: revaiw {name} [on|off]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn init_writes_the_templates_once_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let first = init(d.path());
        assert!(first.iter().all(|(_, w)| *w), "{first:?}");
        assert!(crate::config::parse(&std::fs::read_to_string(d.path().join(".review/config.toml")).unwrap()).is_ok(),
                "the template parses");
        std::fs::write(d.path().join("REVIEW.md"), "mine\n").unwrap();
        assert!(init(d.path()).iter().all(|(_, w)| !*w));
        assert_eq!(std::fs::read_to_string(d.path().join("REVIEW.md")).unwrap(), "mine\n");
    }
    #[test] fn notify_arg_maps_on_off_none_and_rejects_the_rest() {
        let a = |v: &[&str]| notify_arg(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&[]), Ok(None));
        assert_eq!(a(&["on"]), Ok(Some(true)));
        assert_eq!(a(&["off"]), Ok(Some(false)));
        assert!(a(&["maybe"]).is_err());
        assert!(a(&["on", "off"]).is_err());
    }
    #[test] fn guard_arg_maps_on_off_none_and_rejects_the_rest() {
        let a = |v: &[&str]| guard_arg(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&[]), Ok(None));
        assert_eq!(a(&["on"]), Ok(Some(true)));
        assert_eq!(a(&["off"]), Ok(Some(false)));
        assert_eq!(a(&["maybe"]), Err("usage: revaiw guard [on|off]".to_string()));
        assert!(a(&["on", "off"]).is_err());
    }
    #[test] fn path_hint_only_when_the_install_dir_is_not_on_path() {
        let exe = Path::new("/home/u/.cargo/bin/revaiw");
        let h = path_hint(exe, "/usr/bin:/bin").unwrap();
        assert!(h.contains("/home/u/.cargo/bin") && h.contains("export PATH=") && h.contains(".bashrc"), "{h}");
        assert_eq!(path_hint(exe, "/usr/bin:/home/u/.cargo/bin"), None);
        assert_eq!(path_hint(exe, "/home/u/.cargo/bin/:/usr/bin"), None, "a trailing slash is the same dir");
    }
    #[test] fn prereqs_name_each_tool_with_a_hint() {
        let p = prereqs();
        for t in ["claude", "gh", "git"] { assert!(p.iter().any(|(n, _, h)| *n == t && !h.is_empty()), "{t}"); }
    }
}
