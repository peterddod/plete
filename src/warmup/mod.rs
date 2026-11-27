//! Session warm-up cache for agent profiles: a profile's `[warmup]` section,
//! where a warm-up is kept, when it is stale, how one is built, and
//! `plete warmup`. A task does not use one yet.

// Used by `task new`, which follows.
#![allow(dead_code)]

pub mod argv;
pub mod build;
pub mod cache;
pub mod config;
pub mod fresh;
pub mod git;
pub mod lock;

use std::path::{Path, PathBuf};

pub use config::{check, Config};

pub use crate::worktrees::now_secs;

/// `$PLETE_HOME/warmup`, where every warm-up is kept.
pub fn root(home: &Path) -> PathBuf {
    home.join("warmup")
}

/// The local zone's offset from UTC at `at` (unix seconds), in seconds.
pub fn utc_offset(at: i64) -> i64 {
    #[cfg(unix)]
    {
        let time = at as libc::time_t;
        // SAFETY: `localtime_r` fills the struct it is given, and only reads `time`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
            return 0;
        }
        // `i32` or `i64` by platform.
        #[allow(clippy::useless_conversion)]
        i64::from(tm.tm_gmtoff)
    }
    #[cfg(not(unix))]
    {
        let _ = at;
        0
    }
}
