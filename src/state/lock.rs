use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// RAII guard for the exclusive repository lock at `.rusty/lock`.
#[derive(Debug)]
pub struct RepoLock {
    lock_path: PathBuf,
}

impl RepoLock {
    /// Attempt to acquire the exclusive repository lock.
    ///
    /// Fails if:
    /// 1. An active crash recovery is required (`.rusty/operations/active.json` exists).
    /// 2. Another live process currently holds `.rusty/lock`.
    ///
    /// A lock left behind by a process that no longer exists is reclaimed
    /// automatically on Unix. A lock whose owner cannot be determined is never
    /// reclaimed automatically.
    pub fn acquire(rusty_dir: &Path) -> Result<Self> {
        Self::acquire_inner(rusty_dir, true)
    }

    /// Acquire the lock for crash recovery, bypassing the active-journal check.
    ///
    /// The journal check itself is what normally prevents a second command
    /// from running against a half-applied operation; recovery must be allowed
    /// to take the lock so it can finish or undo that operation.
    pub fn acquire_for_recovery(rusty_dir: &Path) -> Result<Self> {
        Self::acquire_inner(rusty_dir, false)
    }

    fn acquire_inner(rusty_dir: &Path, check_journal: bool) -> Result<Self> {
        let operations_dir = rusty_dir.join("operations");
        let active_journal = operations_dir.join("active.json");

        if check_journal && active_journal.exists() {
            bail!(
                "Repository recovery required: an interrupted operation was detected in '{}'. \
                 Run recovery or remove the journal to proceed.",
                active_journal.display()
            );
        }

        let lock_path = rusty_dir.join("lock");

        for attempt in 0..2 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let pid = std::process::id();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let lock_info = format!("pid: {}\ntimestamp: {}\n", pid, now);
                    let _ = file.write_all(lock_info.as_bytes());
                    let _ = file.sync_all();

                    return Ok(Self { lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && lock_is_stale(&lock_path) {
                        // Best-effort reclamation. A racing process that also
                        // noticed the stale lock may remove it first; the
                        // create_new retry below will then either win or report
                        // a live lock.
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }

                    let lock_content = fs::read_to_string(&lock_path).unwrap_or_default();
                    bail!(
                        "Repository is locked by another process ({}). Lock file: '{}'",
                        lock_content.trim(),
                        lock_path.display()
                    );
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to create repository lock at '{}'", lock_path.display())
                    });
                }
            }
        }

        // The loop always returns or bails; this is only a compiler aid.
        bail!("Failed to acquire repository lock at '{}'", lock_path.display())
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let content = fs::read_to_string(lock_path).ok()?;
    for line in content.lines() {
        if let Some(pid) = line.trim().strip_prefix("pid:") {
            return pid.trim().parse::<u32>().ok();
        }
    }
    None
}

fn lock_is_stale(lock_path: &Path) -> bool {
    let Some(pid) = read_lock_pid(lock_path) else {
        return false;
    };

    #[cfg(unix)]
    {
        // Signal 0 performs existence/permission checking without delivering
        // a signal. ESRCH means no such process; EPERM means the process exists
        // but belongs to another user, so the lock must be treated as live.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return false;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::ESRCH)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_lock_acquire_and_release() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir(&rusty_dir).unwrap();

        let lock1 = RepoLock::acquire(&rusty_dir).unwrap();
        assert!(rusty_dir.join("lock").exists());

        // Attempting to acquire again must fail
        assert!(RepoLock::acquire(&rusty_dir).is_err());

        // Dropping lock1 releases it
        drop(lock1);
        assert!(!rusty_dir.join("lock").exists());

        // Now acquiring succeeds again
        let _lock2 = RepoLock::acquire(&rusty_dir).unwrap();
        assert!(rusty_dir.join("lock").exists());
    }

    #[test]
    fn test_lock_fails_when_recovery_active() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        let ops_dir = rusty_dir.join("operations");
        fs::create_dir_all(&ops_dir).unwrap();
        fs::write(ops_dir.join("active.json"), "{}").unwrap();

        let result = RepoLock::acquire(&rusty_dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Repository recovery required"));

        // Recovery is allowed to bypass the journal check and take the lock.
        let recovery_lock = RepoLock::acquire_for_recovery(&rusty_dir).unwrap();
        drop(recovery_lock);
    }

    #[cfg(unix)]
    #[test]
    fn test_stale_lock_is_reclaimed() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir(&rusty_dir).unwrap();

        // PID 0 is never a normal user process; kill(0, 0) checks the process
        // group, so use a very large PID that cannot exist in the test
        // environment instead.
        fs::write(
            rusty_dir.join("lock"),
            "pid: 2147483647\ntimestamp: 0\n",
        )
        .unwrap();

        let lock = RepoLock::acquire(&rusty_dir).unwrap();
        assert!(rusty_dir.join("lock").exists());
        drop(lock);
    }

    #[test]
    fn test_lock_without_pid_is_not_reclaimed() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir(&rusty_dir).unwrap();
        fs::write(rusty_dir.join("lock"), "garbage\n").unwrap();

        assert!(RepoLock::acquire(&rusty_dir).is_err());
    }
}
