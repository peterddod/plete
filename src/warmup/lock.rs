//! Builds of one key take turns, and at most [`MAX_BUILDS`] run at once. The
//! warm-up is not a task: none of this touches `max_live` or a budget.
//!
//! A lock is a file holding its owner's pid, made with `create_new`; one whose
//! process is gone is reclaimed.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Warm-ups running at once, across all keys.
pub const MAX_BUILDS: usize = 2;

/// Where the slots' lock files are, under the `warmup` directory.
const SLOTS: &str = ".slots";

/// A lock file without a pid yet is its owner's, just made, for this long.
const UNWRITTEN_SECS: u64 = 10;

const POLL: Duration = Duration::from_millis(100);

/// A held lock, released when dropped.
pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        // Only our own file: `clear` may have removed the directory, and a
        // waiter made the lock again since.
        let ours = std::fs::read_to_string(&self.0)
            .is_ok_and(|text| text.trim() == std::process::id().to_string());
        if ours {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Remove a dead holder's lock file. Callers that saw it dead take turns
/// (an `flock` on a file beside it) and look again, so a late one does not
/// remove the live lock an earlier one made; nobody makes a lock while the
/// dead one is there, so the look and the removal cannot be split.
fn reclaim(path: &Path) {
    #[cfg(unix)]
    let _turn = {
        use std::os::fd::AsRawFd;
        let mut name = path.as_os_str().to_owned();
        name.push(".reclaim");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(name)
            .ok();
        if let Some(file) = &file {
            // SAFETY: a blocking lock on a descriptor we own, dropped with it.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        }
        file
    };
    if !holder_alive(path) {
        let _ = std::fs::remove_file(path);
    }
}

fn holder_alive(path: &Path) -> bool {
    let age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map_or(0, |d| d.as_secs());
    match std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .parse::<u32>()
    {
        Ok(pid) => crate::review::process_alive(pid),
        Err(_) => age < UNWRITTEN_SECS,
    }
}

/// Take the lock at `path`, or `None` if a live process holds it.
pub fn try_acquire(path: &Path) -> Result<Option<Lock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Twice at most: once as found, once more after clearing a dead one's.
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let lock = Lock(path.to_path_buf());
                writeln!(file, "{}", std::process::id())
                    .with_context(|| format!("write {}", path.display()))?;
                return Ok(Some(lock));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if holder_alive(path) {
                    return Ok(None);
                }
                reclaim(path);
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
    Ok(None)
}

/// Take the lock at `path`, waiting for whoever holds it. A holder is bounded
/// by its warm-up's timeout, and a dead one is reclaimed, so this ends.
pub fn acquire(path: &Path) -> Result<Lock> {
    loop {
        if let Some(lock) = try_acquire(path)? {
            return Ok(lock);
        }
        std::thread::sleep(POLL);
    }
}

/// One of `max` build slots under `root`, waiting for one to free.
fn acquire_slot(root: &Path, max: usize) -> Result<Lock> {
    let dir = root.join(SLOTS);
    loop {
        for n in 0..max {
            if let Some(lock) = try_acquire(&dir.join(format!("{n}.lock")))? {
                return Ok(lock);
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Whether anyone holds the lock at `path` now.
pub fn is_held(path: &Path) -> bool {
    path.exists() && holder_alive(path)
}

/// Where the lock of the warm-up in `dir` is.
pub fn key_lock(dir: &Path) -> PathBuf {
    dir.join(".lock")
}

/// Run `build` for the key kept in `dir` (under `root`, the `warmup`
/// directory) once it holds the key's lock and a slot, if `needed` still says
/// so then: a caller that waited on another's build finds it done and builds
/// nothing. `None`: it was not needed.
pub fn serialised<T>(
    root: &Path,
    dir: &Path,
    max: usize,
    needed: impl FnOnce() -> Result<bool>,
    build: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    let _key = acquire(&key_lock(dir))?;
    if !needed()? {
        return Ok(None);
    }
    let _slot = acquire_slot(root, max)?;
    build().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_held_lock_is_not_given_twice_and_is_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/.lock");
        let first = try_acquire(&path).unwrap().unwrap();
        assert!(is_held(&path));
        assert!(try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(!is_held(&path));
        assert!(try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn a_lock_is_removed_on_drop_only_if_it_is_still_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".lock");
        let ours = try_acquire(&path).unwrap().unwrap();
        // Replaced by another process's, as after `clear` and a waiter.
        std::fs::write(&path, "4194999\n").unwrap();
        drop(ours);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "4194999");
        // Its directory gone: nothing to remove, nothing made.
        let gone = tmp.path().join("d/.lock");
        let lock = try_acquire(&gone).unwrap().unwrap();
        std::fs::remove_dir_all(gone.parent().unwrap()).unwrap();
        drop(lock);
        assert!(!gone.parent().unwrap().exists());
    }

    #[test]
    fn callers_that_all_see_a_dead_holder_do_not_all_get_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Arc::new(tmp.path().join(".lock"));
        for _ in 0..20 {
            std::fs::write(&*path, "4194999\n").unwrap();
            let held = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let (path, held, most) = (path.clone(), held.clone(), most.clone());
                    std::thread::spawn(move || {
                        if let Some(lock) = try_acquire(&path).unwrap() {
                            let now = held.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            held.fetch_sub(1, Ordering::SeqCst);
                            drop(lock);
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(most.load(Ordering::SeqCst), 1);
            let _ = std::fs::remove_file(&*path);
        }
    }

    #[test]
    fn the_lock_of_a_dead_process_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".lock");
        // A pid no process has: above pid_max everywhere plete runs.
        std::fs::write(&path, "4194999\n").unwrap();
        assert!(!is_held(&path));
        let lock = try_acquire(&path).unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(lock);
        // One that is alive (this process) is not.
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert!(try_acquire(&path).unwrap().is_none());
    }

    #[test]
    fn two_callers_of_one_key_build_once_and_both_find_it_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("p/s");
        let built = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (root, dir, built, ran) =
                    (root.clone(), dir.clone(), built.clone(), ran.clone());
                std::thread::spawn(move || {
                    serialised(
                        &root,
                        &dir,
                        MAX_BUILDS,
                        // Fresh once a build has finished.
                        || Ok(built.load(Ordering::SeqCst) == 0),
                        || {
                            ran.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(300));
                            built.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes.iter().filter(|o| o.is_some()).count(), 1);
        assert_eq!(outcomes.iter().filter(|o| o.is_none()).count(), 1);
    }

    #[test]
    fn a_third_key_waits_for_a_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..3)
            .map(|n| {
                let (root, running, most) = (root.clone(), running.clone(), most.clone());
                std::thread::spawn(move || {
                    let dir = root.join(format!("p/s{n}"));
                    serialised(
                        &root,
                        &dir,
                        MAX_BUILDS,
                        || Ok(true),
                        || {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(300));
                            running.fetch_sub(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_some());
        }
        assert_eq!(most.load(Ordering::SeqCst), MAX_BUILDS);
        // Nothing is left held.
        assert!(!is_held(&root.join(".slots/0.lock")));
        assert!(!is_held(&root.join(".slots/1.lock")));
    }

    #[test]
    fn a_failed_build_releases_its_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p/s");
        let out: Result<Option<()>> =
            serialised(tmp.path(), &dir, 1, || Ok(true), || anyhow::bail!("boom"));
        assert!(out.is_err());
        assert!(!is_held(&key_lock(&dir)));
        assert!(!is_held(&tmp.path().join(".slots/0.lock")));
    }
}
