//! One sync at a time per machine.
//!
//! The autosync timer and the `SessionEnd` hook can fire together, and a
//! person can run `sync` while either is running. Two runs in the same sync
//! repository and `~/.claude` at once would race each other's git index and
//! file writes, so every run takes an exclusive lock on a file in the
//! config directory first. The operating system drops the lock when the
//! process ends, however it ends, so a crash never leaves it stuck.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Held for as long as this value lives.
#[derive(Debug)]
pub struct SyncLock {
    _file: File,
}

/// The flag file: `sync.rerun` in the config directory. A run that finds the
/// lock taken leaves it; the holder syncs once more when it finishes, so a
/// turn that ended during a sync is still pushed promptly.
pub fn rerun_flag_path() -> Result<PathBuf> {
    Ok(crate::config::ConfigManager::ensure_config_dir()?.join("sync.rerun"))
}

/// How many extra rounds the lock holder runs for the flag, at most.
pub const MAX_EXTRA_ROUNDS: u32 = 3;

/// Ask the lock holder for another sync after its current one.
pub fn request_rerun() -> Result<()> {
    let path = rerun_flag_path()?;
    std::fs::write(&path, b"")
        .with_context(|| format!("Failed to write the flag {}", path.display()))
}

/// Take the request: whether there was one (and it is now cleared).
pub fn take_rerun() -> Result<bool> {
    let path = rerun_flag_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("Failed to remove the flag {}", path.display())),
    }
}

/// The lock file: `sync.lock` in the config directory.
pub fn lock_path() -> Result<PathBuf> {
    Ok(crate::config::ConfigManager::ensure_config_dir()?.join("sync.lock"))
}

impl SyncLock {
    /// Take the lock if nobody holds it; `None` when another run does.
    pub fn try_acquire() -> Result<Option<SyncLock>> {
        let path = lock_path()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open the lock file {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(SyncLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("Failed to lock {}", path.display()))
            }
        }
    }

    /// Take the lock, waiting up to `timeout` for another run to finish.
    /// `on_wait` runs once, when the lock is first found taken.
    pub fn acquire_waiting(timeout: Duration, on_wait: impl FnOnce()) -> Result<SyncLock> {
        let started = Instant::now();
        let mut on_wait = Some(on_wait);
        loop {
            if let Some(lock) = Self::try_acquire()? {
                return Ok(lock);
            }
            if let Some(say) = on_wait.take() {
                say();
            }
            if started.elapsed() >= timeout {
                anyhow::bail!(
                    "Another claude-code-sync run has held {} for over {} seconds; try again later",
                    lock_path()?.display(),
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn a_second_run_finds_the_lock_taken_until_the_first_ends() {
        let config = tempfile::TempDir::new().unwrap();
        let previous = std::env::var("CLAUDE_CODE_SYNC_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", config.path());

        let first = SyncLock::try_acquire().unwrap().expect("free at first");
        assert!(
            SyncLock::try_acquire().unwrap().is_none(),
            "held by the first"
        );
        let mut waited = false;
        assert!(SyncLock::acquire_waiting(Duration::from_millis(300), || waited = true).is_err());
        assert!(waited);
        drop(first);
        assert!(
            SyncLock::try_acquire().unwrap().is_some(),
            "released on drop"
        );

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial]
    fn the_rerun_flag_is_taken_once() {
        let config = tempfile::TempDir::new().unwrap();
        let previous = std::env::var("CLAUDE_CODE_SYNC_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", config.path());

        assert!(!take_rerun().unwrap(), "nothing asked yet");
        request_rerun().unwrap();
        request_rerun().unwrap();
        assert!(rerun_flag_path().unwrap().is_file());
        assert!(take_rerun().unwrap(), "asked, however many times");
        assert!(!take_rerun().unwrap(), "and cleared");

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR"),
        }
    }
}
