//! Running makima processes, discoverable by any other one without knowing
//! where the binary lives: `<pid>.json` holds the record and `<pid>.lock` is
//! held exclusively while the process lives. The OS releases the lock on exit,
//! so an unlocked sidecar means the owner died and the next reader prunes it.
//! Windows lock ranges also block reads, hence the separate data file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::StateDir;

pub const INSTANCES_DIR: &str = "instances";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceKind {
    Desktop,
    Tui,
    Acp,
}

impl InstanceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Tui => "tui",
            Self::Acp => "acp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub pid: u32,
    pub kind: InstanceKind,
    pub cwd: String,
    #[serde(default)]
    pub session: Option<String>,
    /// Unix seconds.
    pub started: u64,
}

impl Instance {
    pub fn is_self(&self) -> bool {
        self.pid == std::process::id()
    }
}

/// Holds the instance's lock for as long as it lives.
pub struct InstanceGuard {
    lock: Option<File>,
    record: PathBuf,
    lock_path: PathBuf,
}

impl InstanceGuard {
    pub fn register(
        state: &StateDir,
        kind: InstanceKind,
        cwd: &Path,
        session: Option<&str>,
    ) -> io::Result<Self> {
        let dir = state.path().join(INSTANCES_DIR);
        fs::create_dir_all(&dir)?;
        let pid = std::process::id();
        let record = dir.join(format!("{pid}.json"));
        let lock_path = dir.join(format!("{pid}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)?;
        lock.try_lock_exclusive()?;
        let instance = Instance {
            pid,
            kind,
            cwd: cwd.display().to_string(),
            session: session.map(str::to_owned),
            started: now_secs(),
        };
        let body = serde_json::to_vec(&instance).map_err(io::Error::other)?;
        fs::write(&record, body)?;
        Ok(Self {
            lock: Some(lock),
            record,
            lock_path,
        })
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Windows refuses to unlink a file this process still locks.
        self.lock.take();
        let _ = fs::remove_file(&self.record);
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Live instances, newest first.
pub fn list(state: &StateDir) -> Vec<Instance> {
    let dir = state.path().join(INSTANCES_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut instances: Vec<Instance> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| match read_live(&path) {
            Some(instance) => Some(instance),
            None => {
                let _ = fs::remove_file(&path);
                None
            }
        })
        .collect();
    instances.sort_by(|left, right| {
        right
            .started
            .cmp(&left.started)
            .then(left.pid.cmp(&right.pid))
    });
    instances
}

fn read_live(path: &Path) -> Option<Instance> {
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .ok()?;
    if lock.try_lock_exclusive().is_ok() {
        // Nobody holds the lock, so the writer is gone.
        let _ = fs::remove_file(&lock_path);
        return None;
    }
    let mut record = String::new();
    File::open(path).ok()?.read_to_string(&mut record).ok()?;
    serde_json::from_str(&record).ok()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{INSTANCES_DIR, InstanceGuard, InstanceKind, list};
    use crate::StateDir;

    fn state(dir: &TempDir) -> StateDir {
        StateDir::from_path(dir.path().join("state"))
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/project")
    }

    #[test]
    fn a_registered_instance_is_listed() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        let _guard = InstanceGuard::register(&state, InstanceKind::Desktop, &cwd(), None).unwrap();

        let instances = list(&state);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].kind, InstanceKind::Desktop);
        assert_eq!(instances[0].cwd, cwd().display().to_string());
        assert!(instances[0].is_self());
    }

    #[test]
    fn releasing_the_guard_removes_the_entry() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);

        let guard = InstanceGuard::register(&state, InstanceKind::Tui, &cwd(), None).unwrap();
        drop(guard);

        assert!(list(&state).is_empty());
    }

    #[test]
    fn an_unlocked_entry_is_pruned() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        let folder = state.path().join(INSTANCES_DIR);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("424242.json"),
            r#"{"pid":424242,"kind":"tui","cwd":"/gone","started":1}"#,
        )
        .unwrap();
        std::fs::write(folder.join("424242.lock"), "").unwrap();

        assert!(list(&state).is_empty());
        assert!(!folder.join("424242.json").exists());
        assert!(!folder.join("424242.lock").exists());
    }

    #[test]
    fn the_session_is_recorded() {
        let dir = TempDir::new().unwrap();
        let state = state(&dir);
        let _guard =
            InstanceGuard::register(&state, InstanceKind::Acp, &cwd(), Some("session-1")).unwrap();

        assert_eq!(list(&state)[0].session.as_deref(), Some("session-1"));
    }
}
