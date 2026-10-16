use serde::Deserialize;
use std::process::Command;

/// The numbers `~/.claude/hooks/resource-guard.py --json` prints.
#[derive(Deserialize, Debug)]
pub struct Guard {
    pub avail: f64,
    pub load1: f64,
    pub cores: f64,
    pub heavy: u32,
}

/// Slot 1 uses the guard's own thresholds; slot 2 only opens with clear headroom. A third only when the daemon
/// was started with `REVAIW_SLOTS=3` (a temporary catch-up), with slot 2's headroom.
pub fn can_start(g: &Guard, running: usize, max: usize) -> bool {
    if running >= max { return false; }
    match running {
        0 => g.avail >= 3.0 && g.load1 < 0.85 * g.cores && g.heavy < 3,
        1 => g.avail >= 6.0 && g.load1 < 0.50 * g.cores && g.heavy < 2,
        2 => g.avail >= 6.0 && g.load1 < 0.50 * g.cores && g.heavy < 2,
        _ => false,
    }
}

/// Whether a job may start now.
pub enum Admit {
    Go,
    /// Every slot is busy.
    Full,
    /// The resource guard is on and the box is too busy; the reading is for the log line.
    Held(Guard),
    /// The resource guard is on and neither its script nor /proc could be read.
    Unreadable,
}

impl Admit {
    pub fn go(&self) -> bool { matches!(self, Admit::Go) }
}

/// The resource guard is opt-in (`revaiw guard on`). Off, only the slots limit jobs, and nothing is measured: no
/// guard script, no /proc. On, the box has to have room too (`can_start`); `read` measures it, and only when a slot
/// is free.
pub fn admit(enabled: bool, running: usize, max: usize, read: impl FnOnce() -> Option<Guard>) -> Admit {
    if running >= max { return Admit::Full; }
    if !enabled { return Admit::Go; }
    match read() {
        Some(g) if can_start(&g, running, max) => Admit::Go,
        Some(g) => Admit::Held(g),
        None => Admit::Unreadable,
    }
}

/// `REVAIW_SLOTS`, read once at launch: 2 by default, 3 at most.
pub fn slots_from(v: Option<&str>) -> usize {
    v.and_then(|s| s.trim().parse().ok()).map_or(2, |n: usize| n.clamp(2, 3))
}

/// The guard is a personal hook, not part of the repo. Where it is not installed
/// the daemon measures the box itself from /proc, without the guard's count of
/// other heavy runners (heavy = 0).
pub fn read_guard() -> Option<Guard> {
    let script = format!("{}/.claude/hooks/resource-guard.py", std::env::var("HOME").ok()?);
    if std::path::Path::new(&script).exists() {
        let o = Command::new("python3").arg(script).arg("--json").output().ok()?;
        return serde_json::from_slice(&o.stdout).ok();
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    from_proc(&std::fs::read_to_string("/proc/meminfo").ok()?,
              &std::fs::read_to_string("/proc/loadavg").ok()?, cores)
}

fn from_proc(meminfo: &str, loadavg: &str, cores: usize) -> Option<Guard> {
    let kb: f64 = meminfo.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?
        .trim().trim_end_matches("kB").trim().parse().ok()?;
    let load1: f64 = loadavg.split_whitespace().next()?.parse().ok()?;
    Some(Guard { avail: kb / 1048576.0, load1, cores: cores as f64, heavy: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn g(avail: f64, load1: f64, heavy: u32) -> Guard { Guard { avail, load1, cores: 16.0, heavy } }
    #[test] fn slot_one() {
        assert!(can_start(&g(3.0, 13.0, 2), 0, 2));
        assert!(!can_start(&g(2.9, 1.0, 0), 0, 2));
        assert!(!can_start(&g(8.0, 13.7, 0), 0, 2));
        assert!(!can_start(&g(8.0, 1.0, 3), 0, 2));
    }
    #[test] fn slot_two_needs_headroom() {
        assert!(can_start(&g(6.0, 7.9, 1), 1, 2));
        assert!(!can_start(&g(5.9, 1.0, 0), 1, 2));
        assert!(!can_start(&g(8.0, 8.0, 0), 1, 2));
        assert!(!can_start(&g(8.0, 1.0, 2), 1, 2));
    }
    #[test] fn never_three() { assert!(!can_start(&g(15.0, 0.0, 0), 2, 2)); }
    #[test] fn a_third_slot_only_when_asked_and_with_room() {
        assert!(can_start(&g(6.0, 7.9, 1), 2, 3));
        assert!(!can_start(&g(5.9, 1.0, 0), 2, 3));
        assert!(!can_start(&g(15.0, 0.0, 0), 3, 3));
        assert_eq!(slots_from(None), 2);
        assert_eq!(slots_from(Some("3")), 3);
        assert_eq!(slots_from(Some("9")), 3, "capped");
        assert_eq!(slots_from(Some("x")), 2);
    }
    #[test] fn without_the_guard_only_the_slots_limit_jobs_and_nothing_is_measured() {
        let never = || -> Option<Guard> { panic!("the guard must not be read when it is off") };
        assert!(matches!(admit(false, 0, 2, never), Admit::Go));
        assert!(matches!(admit(false, 1, 2, never), Admit::Go), "no headroom check for the second slot");
        assert!(matches!(admit(false, 2, 2, never), Admit::Full));
        assert!(matches!(admit(false, 2, 3, never), Admit::Go), "a third slot opens with REVAIW_SLOTS=3");
        assert!(matches!(admit(false, 3, 3, never), Admit::Full));
    }
    #[test] fn with_the_guard_the_box_decides_too() {
        let busy = || Some(g(2.0, 1.0, 0));
        let idle = || Some(g(15.0, 0.0, 0));
        assert!(matches!(admit(true, 0, 2, idle), Admit::Go));
        assert!(matches!(admit(true, 0, 2, busy), Admit::Held(_)));
        assert!(matches!(admit(true, 1, 2, || Some(g(5.9, 1.0, 0))), Admit::Held(_)), "slot two needs headroom");
        assert!(matches!(admit(true, 0, 2, || None), Admit::Unreadable));
        let never = || -> Option<Guard> { panic!("full slots need no measurement") };
        assert!(matches!(admit(true, 2, 2, never), Admit::Full));
        assert!(admit(true, 0, 2, idle).go() && !admit(true, 0, 2, busy).go() && !admit(false, 2, 2, idle).go());
    }
    #[test] fn measures_from_proc_without_the_guard() {
        let mem = "MemTotal:       16252928 kB\nMemFree:  1000 kB\nMemAvailable:    7340032 kB\n";
        let gd = from_proc(mem, "1.62 1.40 1.20 2/1500 12345", 16).unwrap();
        assert_eq!(gd.avail, 7.0);
        assert_eq!(gd.load1, 1.62);
        assert_eq!(gd.cores, 16.0);
        assert_eq!(gd.heavy, 0);
        assert!(from_proc("MemTotal: 1 kB\n", "1.0 1.0 1.0", 16).is_none());
    }
    #[test] fn parses_guard_json() {
        let j = r#"{"avail": 7.3, "total": 15.5, "load1": 0.6, "cores": 16, "heavy": 1}"#;
        let gd: Guard = serde_json::from_str(j).unwrap();
        assert_eq!(gd.heavy, 1);
    }
}
