//! Pid file guarded by an advisory lock.
//!
//! `umsp serve` holds an exclusive `flock` on its pid file for as long as it
//! runs. A CLI command that wants to signal the server first checks that the
//! lock is held: a pid file left behind by a crashed server is not locked, so
//! its (possibly reused) pid is never signalled. This works the same on Linux
//! and macOS and does not depend on `/proc`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::{Result, UmspError};

/// Exclusive ownership of the pid file. Dropping it removes the file and
/// releases the lock.
#[derive(Debug)]
pub struct PidLock {
    path: PathBuf,
    /// Never read: keeping the descriptor open is what holds the lock.
    _file: File,
}

impl PidLock {
    /// Lock `path` and write the current pid into it.
    ///
    /// Fails with `ALREADY_RUNNING` when another live process holds the lock.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                UmspError::other(
                    "PID_FILE",
                    format!("cannot create {}: {e}", parent.display()),
                )
            })?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| {
                UmspError::other("PID_FILE", format!("cannot open {}: {e}", path.display()))
            })?;
        if !try_flock(&file, libc::LOCK_EX) {
            let owner = read_pid(&mut file)
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default();
            return Err(UmspError::other(
                "ALREADY_RUNNING",
                format!(
                    "another umsp serve{owner} holds {}; stop it or use a different state_dir",
                    path.display()
                ),
            ));
        }
        let pid = format!("{}\n", std::process::id());
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| file.write_all(pid.as_bytes()))
            .and_then(|()| file.flush())
            .map_err(|e| {
                UmspError::other("PID_FILE", format!("cannot write {}: {e}", path.display()))
            })?;
        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        // Remove while still holding the lock so no reader can observe an
        // unlocked file that still names this pid.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Pid of the server that currently holds the lock on `path`, if any.
///
/// Returns `None` when the file is missing, unreadable, or not locked (a stale
/// file left by a server that exited without cleaning up).
pub fn locked_pid(path: &Path) -> Option<i32> {
    let mut file = File::open(path).ok()?;
    if try_flock(&file, libc::LOCK_SH) {
        // Nobody holds the exclusive lock: the pid inside is stale. The shared
        // lock is released when `file` is dropped.
        return None;
    }
    read_pid(&mut file)
}

fn read_pid(file: &mut File) -> Option<i32> {
    let mut raw = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut raw).ok()?;
    raw.trim().parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// Non-blocking `flock`. `true` means the lock was taken.
fn try_flock(file: &File, op: libc::c_int) -> bool {
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) == 0 }
}

/// Send SIGHUP to `pid`. `true` when the signal was delivered.
pub fn send_hangup(pid: i32) -> bool {
    // SAFETY: `kill` has no memory-safety preconditions.
    unsafe { libc::kill(pid, libc::SIGHUP) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umsp-pidfile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("umsp.pid")
    }

    #[test]
    fn missing_file_has_no_owner() {
        let path = temp_path("missing");
        assert_eq!(locked_pid(&path), None);
    }

    #[test]
    fn stale_unlocked_file_is_never_reported() {
        let path = temp_path("stale");
        // A crashed server left its pid behind; that pid may now belong to an
        // unrelated process and must not be signalled.
        std::fs::write(&path, "1\n").unwrap();
        assert_eq!(locked_pid(&path), None);
    }

    #[test]
    fn held_lock_reports_the_owner_and_blocks_a_second_server() {
        let path = temp_path("held");
        let lock = PidLock::acquire(&path).unwrap();
        assert_eq!(locked_pid(&path), Some(std::process::id() as i32));

        let err = PidLock::acquire(&path).unwrap_err();
        assert_eq!(err.code, "ALREADY_RUNNING");

        drop(lock);
        assert!(!path.exists(), "pid file is removed on drop");
        assert_eq!(locked_pid(&path), None);
        // The lock is free again.
        drop(PidLock::acquire(&path).unwrap());
    }

    #[test]
    fn acquire_overwrites_a_stale_longer_pid() {
        let path = temp_path("overwrite");
        std::fs::write(&path, "99999999999\n").unwrap();
        let _lock = PidLock::acquire(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, format!("{}\n", std::process::id()));
    }
}
