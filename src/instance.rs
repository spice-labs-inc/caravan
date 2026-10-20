//! Single-active-instance detection. A second caravan on the same machine
//! (a dev build next to the installed one) runs passive: it reads the shared
//! cache and mirror but never spends the GitHub API budget.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LOCK_FILE_NAME: &str = "caravan.lock";
const STALE_AFTER: Duration = Duration::from_secs(180);
const HEARTBEAT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunMode {
    #[default]
    Active,
    Passive {
        reason: String,
    },
}

impl RunMode {
    pub fn is_active(&self) -> bool {
        matches!(self, RunMode::Active)
    }
}

/// Removes the lock file when dropped. Held by the app for its lifetime.
#[derive(Debug)]
pub struct InstanceGuard {
    path: PathBuf,
}

impl InstanceGuard {
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the lock fresh so other instances keep seeing us as alive.
    pub fn spawn_heartbeat(&self, runtime: &tokio::runtime::Runtime) {
        let path = self.path.clone();
        let pid = std::process::id();
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                if let Err(e) = std::fs::write(&path, pid.to_string()) {
                    tracing::warn!(err = %e, "Failed to touch instance lock");
                }
            }
        });
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            if content.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Decide whether this process runs active or passive. Active returns the
/// guard that owns the lock file.
pub fn detect(data_dir: &Path) -> (RunMode, Option<InstanceGuard>) {
    if std::env::var("CARAVAN_GITHUB")
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return (
            RunMode::Passive {
                reason: "CARAVAN_GITHUB=off".to_string(),
            },
            None,
        );
    }
    detect_lock(data_dir, std::process::id())
}

fn detect_lock(data_dir: &Path, our_pid: u32) -> (RunMode, Option<InstanceGuard>) {
    let path = data_dir.join(LOCK_FILE_NAME);
    if let Ok(meta) = std::fs::metadata(&path) {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < STALE_AFTER);
        let holder = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if fresh && holder.is_some_and(|pid| pid != our_pid) {
            return (
                RunMode::Passive {
                    reason: format!("another caravan is running (pid {})", holder.unwrap_or(0)),
                },
                None,
            );
        }
    }
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        tracing::warn!(err = %e, "Failed to create data dir for instance lock");
    }
    match std::fs::write(&path, our_pid.to_string()) {
        Ok(()) => (RunMode::Active, Some(InstanceGuard { path })),
        Err(e) => {
            tracing::warn!(err = %e, "Failed to write instance lock, running active anyway");
            (RunMode::Active, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::var_os("CARAVAN_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("caravan-test-lock-{}-{}", name, std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_lock_means_active_and_writes_pid() {
        let dir = scratch("fresh");
        let (mode, guard) = detect_lock(&dir, 4242);
        assert_eq!(mode, RunMode::Active);
        assert!(guard.is_some());
        assert_eq!(
            std::fs::read_to_string(dir.join(LOCK_FILE_NAME)).unwrap(),
            "4242"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_lock_from_other_pid_means_passive() {
        let dir = scratch("other");
        std::fs::write(dir.join(LOCK_FILE_NAME), "1234").unwrap();
        let (mode, guard) = detect_lock(&dir, 4242);
        assert_eq!(
            mode,
            RunMode::Passive {
                reason: "another caravan is running (pid 1234)".to_string()
            }
        );
        assert!(guard.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn own_pid_lock_means_active() {
        let dir = scratch("own");
        std::fs::write(dir.join(LOCK_FILE_NAME), "4242").unwrap();
        let (mode, _guard) = detect_lock(&dir, 4242);
        assert_eq!(mode, RunMode::Active);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let dir = scratch("stale");
        let path = dir.join(LOCK_FILE_NAME);
        std::fs::write(&path, "1234").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (mode, guard) = detect_lock(&dir, 4242);
        assert_eq!(mode, RunMode::Active);
        assert!(guard.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn guard_drop_removes_own_lock_only() {
        let dir = scratch("drop");
        let path = dir.join(LOCK_FILE_NAME);
        let own = std::process::id();
        let (_, guard) = detect_lock(&dir, own);
        drop(guard);
        assert!(!path.exists());

        std::fs::write(&path, "1").unwrap();
        drop(InstanceGuard { path: path.clone() });
        assert!(path.exists(), "a lock held by someone else must survive");
        std::fs::remove_dir_all(&dir).ok();
    }
}
