//! Where a warm-up is kept: `$PLETE_HOME/warmup/<profile>/<scope>/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const META: &str = "meta.json";
const CONTEXT: &str = "CONTEXT.md";
const WORK: &str = "work";

/// What is known of a built base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// The base session's id.
    pub base_session: String,
    /// Unix seconds.
    pub built_at: i64,
    /// The default branch's commit each repo was built at, by its directory
    /// under `work/`.
    pub commits: BTreeMap<String, String>,
    pub fingerprint: String,
    /// Files the warm-up read, relative to `work/` (so `<repo>/<path>`). Empty
    /// when the harness could not say.
    #[serde(default)]
    pub files_read: Vec<String>,
    pub turns: u32,
    pub duration_secs: u64,
    /// Input tokens of the base's last turn; none when unknown.
    #[serde(default)]
    pub base_tokens: Option<u64>,
}

/// `name` if it is one safe directory name: a profile, or a repo's store id or
/// a project's name as a scope.
pub fn safe_name(name: &str) -> Result<&str> {
    let bad = name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control);
    if bad {
        bail!("{name:?} is not a name a warm-up can be kept under");
    }
    Ok(name)
}

/// The directory of one warm-up, `root` being [`crate::config::warmup_root`].
pub fn dir(root: &Path, profile: &str, scope: &str) -> Result<PathBuf> {
    Ok(root.join(safe_name(profile)?).join(safe_name(scope)?))
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join(META)
}

pub fn context_path(dir: &Path) -> PathBuf {
    dir.join(CONTEXT)
}

/// The warm-up's cwd: worktrees of its repos.
pub fn work_dir(dir: &Path) -> PathBuf {
    dir.join(WORK)
}

/// The base's meta, or none when it is missing or unreadable: such a base is
/// stale, not an error.
pub fn read_meta(dir: &Path) -> Option<Meta> {
    let text = std::fs::read_to_string(meta_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `meta` by renaming a temp file over `meta.json`, so a reader never
/// sees half of it.
pub fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!("{META}.tmp"));
    let text = serde_json::to_string_pretty(meta).context("write meta")?;
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, meta_path(dir)).with_context(|| format!("write {}", META))
}

pub fn read_context(dir: &Path) -> Option<String> {
    std::fs::read_to_string(context_path(dir)).ok()
}

pub fn write_context(dir: &Path, text: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = context_path(dir);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta {
            base_session: "b".into(),
            built_at: 100,
            commits: BTreeMap::from([("jeet".into(), "abc".into())]),
            fingerprint: "f".into(),
            files_read: vec!["jeet/src/a.rs".into()],
            turns: 7,
            duration_secs: 42,
            base_tokens: Some(1234),
        }
    }

    #[test]
    fn names_are_one_safe_component() {
        for ok in ["planner", "github.com_peterddod_jeet", "my-proj"] {
            assert_eq!(safe_name(ok).unwrap(), ok);
        }
        for bad in [
            "", ".", "..", ".hidden", "a/b", "../x", "a\\b", "a\0b", "a\nb",
        ] {
            assert!(safe_name(bad).is_err(), "{bad:?}");
        }
        let root = Path::new("/h/warmup");
        assert_eq!(
            dir(root, "p", "s").unwrap(),
            Path::new("/h/warmup/p/s").to_path_buf()
        );
        assert!(dir(root, "p", "../s").is_err());
        assert!(dir(root, "../p", "s").is_err());
    }

    #[test]
    fn meta_and_context_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(tmp.path(), "p", "s").unwrap();
        assert_eq!(read_meta(&d), None);
        assert_eq!(read_context(&d), None);
        write_meta(&d, &meta()).unwrap();
        write_context(&d, "# map\n").unwrap();
        assert_eq!(read_meta(&d), Some(meta()));
        assert_eq!(read_context(&d).as_deref(), Some("# map\n"));
        assert!(!d.join("meta.json.tmp").exists());
        assert_eq!(work_dir(&d), d.join("work"));
    }

    #[test]
    fn a_corrupt_meta_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(meta_path(tmp.path()), "{not json").unwrap();
        assert_eq!(read_meta(tmp.path()), None);
    }
}
