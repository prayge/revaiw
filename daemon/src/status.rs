use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Run {
    pub branch: String,
    pub head: String,
    pub model: String,
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Done {
    pub branch: String,
    pub head: String,
    pub model: String,
    pub effort: String,
    pub reason: String,
    pub important: usize,
    pub nits: usize,
    pub pre_existing: usize,
    pub md: String,
    pub finished_at: u64,
    /// The open PR for the branch when the review finished; absent in files written before it was recorded.
    #[serde(default)]
    pub pr: Option<u64>,
}

/// What the statusline, the chat hook and `status` read. Written by the daemon only.
#[derive(Serialize, Deserialize, Default)]
pub struct Status {
    pub pid: u32,
    pub running: Vec<Run>,
    pub completed: Vec<Done>,
    pub failed: u32,
    pub paused_until: u64,
    pub day: u64,
    pub done_today: u32,
}

impl Status {
    pub fn load(path: &Path) -> Status {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) {
        write_atomic(path, &serde_json::to_vec_pretty(self).unwrap());
    }
    pub fn push_done(&mut self, d: Done) {
        let day = d.finished_at / 86_400;
        if day != self.day { self.day = day; self.done_today = 0; }
        self.done_today += 1;
        self.completed.push(d);
        let over = self.completed.len().saturating_sub(50);
        self.completed.drain(..over);
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn done(at: u64) -> Done {
        Done { branch: "b".into(), head: "h".into(), model: "m".into(), effort: "e".into(), reason: "r".into(),
               important: 0, nits: 0, pre_existing: 0, md: String::new(), finished_at: at, pr: None }
    }
    #[test] fn a_status_written_before_pr_numbers_still_loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        std::fs::write(&p, r#"{"pid":1,"running":[],"completed":[{"branch":"b","head":"h","model":"m","effort":"e",
            "reason":"r","important":1,"nits":0,"pre_existing":0,"md":"x.md","finished_at":5}],"failed":0,
            "paused_until":0,"day":0,"done_today":1}"#).unwrap();
        let s = Status::load(&p);
        assert_eq!(s.completed.len(), 1);
        assert_eq!(s.completed[0].pr, None);
    }
    #[test] fn caps_history_and_counts_today() {
        let mut s = Status::default();
        for i in 0..60 { s.push_done(done(86_400 * 3 + i)); }
        assert_eq!(s.completed.len(), 50);
        assert_eq!(s.done_today, 60);
        s.push_done(done(86_400 * 4));
        assert_eq!(s.done_today, 1);
    }
}
