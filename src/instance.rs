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
    heartbeat: Option<tokio::task::AbortHandle>,
}

impl InstanceGuard {
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the lock fresh so other instances keep seeing us as alive.
    pub fn spawn_heartbeat(&mut self, runtime: &tokio::runtime::Runtime) {
        let path = self.path.clone();
        let pid = std::process::id();
        let task = runtime.spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                if let Err(e) = std::fs::write(&path, pid.to_string()) {
                    tracing::warn!(err = %e, "Failed to touch instance lock");
                }
            }
        });
        self.heartbeat = Some(task.abort_handle());
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
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
        if fresh && holder.is_some_and(|pid| pid != our_pid && pid_alive(pid)) {
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
        Ok(()) => (
            RunMode::Active,
            Some(InstanceGuard {
                path,
                heartbeat: None,
            }),
        ),
        Err(e) => {
            tracing::warn!(err = %e, "Failed to write instance lock, running active anyway");
            (RunMode::Active, None)
        }
    }
}

/// Whether a process with this pid exists. A lock left by a killed instance
/// names a dead pid and must not keep the next launch passive.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
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
        let other = std::process::id();
        std::fs::write(dir.join(LOCK_FILE_NAME), other.to_string()).unwrap();
        let (mode, guard) = detect_lock(&dir, other + 1);
        assert_eq!(
            mode,
            RunMode::Passive {
                reason: format!("another caravan is running (pid {})", other)
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
        drop(InstanceGuard {
            path: path.clone(),
            heartbeat: None,
        });
        assert!(path.exists(), "a lock held by someone else must survive");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_tells_live_from_dead() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(dead_pid()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn fresh_lock_from_dead_pid_is_taken_over() {
        let dir = scratch("dead");
        let path = dir.join(LOCK_FILE_NAME);
        std::fs::write(&path, dead_pid().to_string()).unwrap();
        let (mode, guard) = detect_lock(&dir, 4242);
        assert_eq!(mode, RunMode::Active);
        assert!(guard.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242");
        std::fs::remove_dir_all(&dir).ok();
    }
}
