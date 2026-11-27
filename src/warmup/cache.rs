//! Where a warm-up is kept: `$PLETE_HOME/warmup/<profile>/<scope>/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

/// `name` if it is one safe directory name: a profile, or a repo's store id
/// or a project's name as a scope.
pub fn safe_name(name: &str) -> Result<&str> {
    crate::projects::check_name(name)
        .map_err(|_| anyhow::anyhow!("{name:?} is not a name a warm-up can be kept under"))?;
    Ok(name)
}

/// The directory of one warm-up, `root` being the `warmup` directory of the plete home.
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
    let meta: Meta = serde_json::from_str(&text).ok()?;
    // Later steps use these in paths and argv, so a hand-edited or damaged
    // file must not smuggle in anything but plain names.
    let plain = uuid::Uuid::parse_str(&meta.base_session).is_ok()
        && meta.commits.keys().all(|k| safe_name(k).is_ok())
        && meta
            .commits
            .values()
            .all(|c| c.bytes().all(|b| b.is_ascii_hexdigit()));
    plain.then_some(meta)
}

/// Write `text` to `dir/name` by renaming a temp file over it, so a reader
/// never sees half of it. The temp name holds the pid, so two writers do not
/// share one.
fn write_atomic(dir: &Path, name: &str, text: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(name)).with_context(|| format!("write {name}"))
}

pub fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let text = serde_json::to_string_pretty(meta).context("write meta")?;
    write_atomic(dir, META, &text)
}

pub fn read_context(dir: &Path) -> Option<String> {
    std::fs::read_to_string(context_path(dir)).ok()
}

pub fn write_context(dir: &Path, text: &str) -> Result<()> {
    write_atomic(dir, CONTEXT, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta {
            base_session: "6f1c1c3e-5b0a-4c53-9a53-2f0f8f3a9d11".into(),
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
        let left: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // A rewrite replaces.
        write_context(&d, "# new\n").unwrap();
        assert_eq!(read_context(&d).as_deref(), Some("# new\n"));
        assert_eq!(work_dir(&d), d.join("work"));
    }

    #[test]
    fn an_older_meta_without_optional_fields_still_reads() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            meta_path(tmp.path()),
            r#"{"base_session":"6f1c1c3e-5b0a-4c53-9a53-2f0f8f3a9d11","built_at":1,"commits":{"r":"abc"},"fingerprint":"f","turns":2,"duration_secs":3}"#,
        )
        .unwrap();
        let m = read_meta(tmp.path()).unwrap();
        assert!(m.files_read.is_empty());
        assert_eq!(m.base_tokens, None);
    }

    #[test]
    fn a_meta_with_unsafe_names_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_meta(d, &meta()).unwrap();
        assert!(read_meta(d).is_some());
        let with = |edit: fn(&mut Meta)| {
            let mut m = meta();
            edit(&mut m);
            write_meta(d, &m).unwrap();
            read_meta(d)
        };
        assert_eq!(with(|m| m.base_session = "../x".into()), None);
        assert_eq!(with(|m| m.base_session = "--resume".into()), None);
        assert_eq!(with(|m| m.base_session = "b".into()), None);
        assert_eq!(with(|m| m.base_session = "".into()), None);
        assert_eq!(with(|m| m.base_session = "a b".into()), None);
        assert_eq!(
            with(|m| {
                m.commits.insert("../r".into(), "abc".into());
            }),
            None
        );
        assert_eq!(
            with(|m| {
                m.commits.insert("r".into(), "--all".into());
            }),
            None
        );
    }

    #[test]
    fn a_corrupt_meta_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(meta_path(tmp.path()), "{not json").unwrap();
        assert_eq!(read_meta(tmp.path()), None);
    }
}
